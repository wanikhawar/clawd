//! The speech-bubble card: chat transcript + prompt box, or a permission prompt.
//!
//! The card only displays things and reports user actions on a channel; the
//! app decides what they mean.

use crate::app::Action;
use crate::markdown;
use crate::projects;
use crate::claude::protocol::{tool_summary, Decision, ModelInfo, PermissionRequest, PlanLimit, SlashCommand};
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, pango, Align, DrawingArea, GestureDrag, LevelBar, ListBox, Popover, ProgressBar, Button, DropDown, Entry, EntryIconPosition, EventControllerKey, Label,
    MenuButton, Orientation, Overflow, Picture, PolicyType, ScrolledWindow, Stack, StringList,
};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const WIDTH: i32 = 380;
/// How long "Hold to allow" must be held for a critical action.
const HOLD: Duration = Duration::from_millis(900);
/// Answers are ignored this soon after a permission card appears: it can pop
/// up under the pointer, and a click (or a drag) meant for another app must
/// never approve anything.
const ANSWER_GUARD: Duration = Duration::from_millis(1200);
/// Moving further than this while holding cancels the hold (it's a drag).
const HOLD_SLOP: f64 = 6.0;

/// Claude Code's permission modes: (value, short label, description).
pub const MODES: [(&str, &str, &str); 6] = [
    ("default", "Ask first", "Asks before running commands or editing files"),
    ("acceptEdits", "Accept edits", "Edits files in the project without asking; asks for commands"),
    ("auto", "Auto", "Claude Code decides what's safe and only asks about the rest"),
    ("plan", "Plan only", "Reads and plans; doesn't change anything"),
    ("dontAsk", "Don't ask", "Never asks: anything that would need permission is denied"),
    ("bypassPermissions", "Bypass", "Runs everything without asking"),
];
/// Tallest the card can get (header + transcript + attachments + prompt box +
/// footer), used to decide whether it fits above Clawd.
pub const MAX_HEIGHT: i32 = 548;
const TRANSCRIPT_MAX_HEIGHT: i32 = 300;

pub struct Card {
    pub root: gtk4::Box,
    stack: Stack,
    title: Label,
    status: Label,
    stop: Button,
    open_in_app: Button,
    folder: MenuButton,
    folder_label: Label,
    /// The project folder, for the folder menu.
    project: Rc<RefCell<PathBuf>>,
    /// The conversation in progress, left out of the folder menu's list.
    session: Rc<RefCell<Option<String>>>,
    transcript: gtk4::Box,
    scroller: ScrolledWindow,
    pub entry: Entry,
    /// The assistant label currently receiving streamed text, and its raw Markdown.
    current: RefCell<Option<(Label, String)>>,
    model_picker: DropDown,
    models: Rc<RefCell<Vec<ModelInfo>>>,
    effort_picker: DropDown,
    /// Picker entries; `None` is "Auto" (the model's default effort).
    efforts: Rc<RefCell<Vec<Option<String>>>>,
    /// Set while pickers are filled in code, so that isn't taken as a choice.
    filling: Rc<Cell<bool>>,
    attachments: gtk4::Box,
    actions: async_channel::Sender<Action>,

    perm_count: Label,
    perm_heading: Label,
    perm_detail: Label,
    perm_note: Label,
    always: Button,
    deny: Button,
    /// ID of the permission request currently on screen.
    shown_request: Rc<RefCell<String>>,
    /// When it appeared (answers are ignored for ANSWER_GUARD after).
    shown_at: Rc<Cell<Instant>>,
    allow: Button,
    tasks: gtk4::Box,
    meter: gtk4::Box,
    meter_label: Label,
    meter_pie: DrawingArea,
    /// (fraction used, level: 0 ok, 1 getting full, 2 nearly full).
    meter_fill: Rc<Cell<(f64, u8)>>,
    commands: Rc<RefCell<Vec<SlashCommand>>>,
    plan: gtk4::Box,
    hold: gtk4::Box,
    hold_bar: ProgressBar,
    mode_picker: DropDown,
}

impl Card {
    pub fn new(actions: async_channel::Sender<Action>) -> Card {
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.add_css_class("card");
        root.set_width_request(WIDTH);

        let stack = Stack::new();
        stack.set_vhomogeneous(false);
        stack.set_interpolate_size(true);
        root.append(&stack);

        // ---- Chat view ----
        let chat = gtk4::Box::new(Orientation::Vertical, 8);

        let header = gtk4::Box::new(Orientation::Horizontal, 6);
        let title = Label::new(None);
        title.add_css_class("title");
        let status = Label::new(None);
        status.add_css_class("dim");
        status.set_hexpand(true);
        status.set_xalign(0.0);
        status.set_ellipsize(pango::EllipsizeMode::End);
        let stop = Button::with_label("Stop");
        stop.add_css_class("stop");
        stop.set_visible(false);
        let new_session = Button::from_icon_name("list-add-symbolic");
        new_session.add_css_class("flat");
        new_session.set_tooltip_text(Some("New session"));
        let open_in_app = Button::from_icon_name("window-new-symbolic");
        open_in_app.add_css_class("flat");
        open_in_app.set_tooltip_text(Some("Continue this conversation in the Claude app"));
        open_in_app.set_sensitive(false);
        let settings = Button::from_icon_name("emblem-system-symbolic");
        settings.add_css_class("flat");
        settings.set_tooltip_text(Some("Settings"));
        let close = Button::with_label("✕");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Hide (Esc)"));
        // The project folder: shows where Claude works, and changes it.
        let folder = MenuButton::new();
        folder.add_css_class("folder");
        let folder_row = gtk4::Box::new(Orientation::Horizontal, 4);
        folder_row.append(&gtk4::Image::from_icon_name("folder-symbolic"));
        let folder_label = Label::new(None);
        folder_label.set_ellipsize(pango::EllipsizeMode::Middle);
        folder_label.set_max_width_chars(16);
        folder_row.append(&folder_label);
        folder.set_child(Some(&folder_row));
        let project: Rc<RefCell<PathBuf>> = Rc::default();
        let session: Rc<RefCell<Option<String>>> = Rc::default();
        let project_menu = gio::Menu::new();
        folder.set_menu_model(Some(&project_menu));
        // Rebuilt each time it opens, so recent projects stay current.
        folder.connect_active_notify({
            let (project, session) = (project.clone(), session.clone());
            move |button| {
                if button.is_active() {
                    fill_project_menu(&project_menu, &project.borrow(), session.borrow().as_deref());
                }
            }
        });

        header.append(&title);
        header.append(&folder);
        header.append(&status);
        // Context usage: a small pie and percentage; details on hover.
        let meter = gtk4::Box::new(Orientation::Horizontal, 4);
        meter.add_css_class("meter");
        let meter_fill = Rc::new(Cell::new((0.0f64, 0u8)));
        let meter_pie = DrawingArea::new();
        meter_pie.set_content_width(12);
        meter_pie.set_content_height(12);
        meter_pie.set_valign(Align::Center);
        meter_pie.set_draw_func({
            let fill = meter_fill.clone();
            move |_, cr, w, h| draw_pie(cr, w as f64, h as f64, fill.get())
        });
        let meter_label = Label::new(None);
        meter.append(&meter_pie);
        meter.append(&meter_label);
        meter.set_visible(false);
        header.append(&meter);
        header.append(&stop);
        header.append(&new_session);
        header.append(&open_in_app);
        header.append(&settings);
        header.append(&close);
        chat.append(&header);

        // Claude's task checklist, while there is one.
        let tasks = gtk4::Box::new(Orientation::Vertical, 2);
        tasks.add_css_class("tasks");
        tasks.set_visible(false);
        chat.append(&tasks);

        let transcript = gtk4::Box::new(Orientation::Vertical, 6);
        // Room on the right for the (overlay) scrollbar, so it never covers
        // your right-aligned messages.
        transcript.set_margin_end(12);
        let scroller = ScrolledWindow::new();
        scroller.set_hscrollbar_policy(PolicyType::Never);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(TRANSCRIPT_MAX_HEIGHT);
        scroller.set_child(Some(&transcript));
        // Hidden until there's something to show, so an empty chat has no gap.
        scroller.set_visible(false);
        // Stick to the newest message as content grows.
        let vadj = scroller.vadjustment();
        vadj.connect_upper_notify(|adj| adj.set_value(adj.upper() - adj.page_size()));
        chat.append(&scroller);

        let attachments = gtk4::Box::new(Orientation::Horizontal, 6);
        attachments.set_visible(false);
        chat.append(&attachments);

        let entry = Entry::new();
        entry.set_placeholder_text(Some("Ask Claude…"));
        entry.set_icon_from_icon_name(EntryIconPosition::Secondary, Some("camera-photo-symbolic"));
        entry.set_icon_tooltip_text(EntryIconPosition::Secondary, Some("Screenshot a region (or paste an image)"));
        chat.append(&entry);

        let footer = gtk4::Box::new(Orientation::Horizontal, 6);
        footer.add_css_class("footer");
        let model_picker = DropDown::from_strings(&["Loading models…"]);
        model_picker.set_tooltip_text(Some("Model"));
        model_picker.set_sensitive(false);
        let effort_picker = DropDown::from_strings(&[]);
        effort_picker.set_tooltip_text(Some("Effort: how much Claude thinks before answering"));
        effort_picker.set_visible(false);
        let mode_names: Vec<String> = MODES.iter().map(|(_, label, _)| label.to_string()).collect();
        let mode_names: Vec<&str> = mode_names.iter().map(String::as_str).collect();
        let mode_picker = DropDown::from_strings(&mode_names);
        footer.append(&model_picker);
        footer.append(&effort_picker);
        footer.append(&mode_picker);
        chat.append(&footer);

        // Plan usage: one slim bar per window (5-hour, weekly, …).
        let plan = gtk4::Box::new(Orientation::Horizontal, 12);
        plan.add_css_class("plan");
        plan.set_visible(false);
        chat.append(&plan);

        stack.add_named(&chat, Some("chat"));

        // ---- Permission view ----
        let perm = gtk4::Box::new(Orientation::Vertical, 8);
        let perm_count = Label::new(None);
        perm_count.add_css_class("dim");
        perm_count.set_xalign(0.0);
        let perm_heading = Label::new(None);
        perm_heading.add_css_class("heading");
        perm_heading.set_xalign(0.0);
        perm_heading.set_wrap(true);

        let perm_detail = Label::new(None);
        perm_detail.add_css_class("code");
        perm_detail.set_xalign(0.0);
        perm_detail.set_wrap(true);
        perm_detail.set_wrap_mode(pango::WrapMode::WordChar);
        perm_detail.set_selectable(true);
        let detail_scroller = ScrolledWindow::new();
        detail_scroller.set_hscrollbar_policy(PolicyType::Never);
        detail_scroller.set_propagate_natural_height(true);
        detail_scroller.set_max_content_height(240);
        detail_scroller.set_child(Some(&perm_detail));

        let perm_note = Label::new(None);
        perm_note.add_css_class("dim");
        perm_note.set_xalign(0.0);
        perm_note.set_wrap(true);

        let buttons = gtk4::Box::new(Orientation::Horizontal, 6);
        buttons.set_halign(Align::End);
        let deny = Button::with_label("Deny");
        deny.add_css_class("deny");
        let always = Button::with_label("Always allow");
        let allow = Button::with_label("Allow");
        allow.add_css_class("allow");
        // Critical actions: press and hold (mouse only; not focusable, so
        // Enter or Space can never approve one).
        let hold = gtk4::Box::new(Orientation::Horizontal, 0);
        hold.add_css_class("hold");
        hold.append(&Label::new(Some("Hold to allow")));
        hold.set_tooltip_text(Some("Press and hold to approve this critical action"));
        hold.set_visible(false);
        let hold_bar = ProgressBar::new();
        hold_bar.add_css_class("hold-bar");
        hold_bar.set_visible(false);
        buttons.append(&deny);
        buttons.append(&always);
        buttons.append(&allow);
        buttons.append(&hold);

        perm.append(&perm_count);
        perm.append(&perm_heading);
        perm.append(&detail_scroller);
        perm.append(&perm_note);
        perm.append(&buttons);
        perm.append(&hold_bar);
        stack.add_named(&perm, Some("permission"));

        // ---- Wiring: every interaction becomes an Action ----
        let on_click = |button: &Button, action: Action| {
            let actions = actions.clone();
            button.connect_clicked(move |_| {
                let _ = actions.send_blocking(action.clone());
            });
        };
        on_click(&stop, Action::Stop);
        on_click(&close, Action::Hide);
        on_click(&open_in_app, Action::ContinueInApp);
        on_click(&settings, Action::OpenSettings);
        on_click(&new_session, Action::NewSession);

        // Answers name the request on screen, so a click can never land on a
        // different request that replaced it (e.g. after Claude withdrew it).
        let shown_request: Rc<RefCell<String>> = Rc::default();
        let shown_at = Rc::new(Cell::new(Instant::now()));
        for (button, decision) in [(&deny, Decision::Deny), (&always, Decision::AlwaysAllow), (&allow, Decision::Allow)] {
            let (actions, shown, shown_at) = (actions.clone(), shown_request.clone(), shown_at.clone());
            button.connect_clicked(move |_| {
                if shown_at.get().elapsed() < ANSWER_GUARD {
                    return; // too soon: probably a click meant for something else
                }
                let _ = actions.send_blocking(Action::Answer(shown.borrow().clone(), decision));
            });
        }

        let held: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        // A drag gesture, so movement can cancel: only a still press counts.
        let press = GestureDrag::new();
        press.set_button(gdk::BUTTON_PRIMARY);
        press.connect_drag_begin({
            let (actions, shown, bar, held, shown_at) =
                (actions.clone(), shown_request.clone(), hold_bar.clone(), held.clone(), shown_at.clone());
            move |_, _, _| {
                if shown_at.get().elapsed() < ANSWER_GUARD {
                    return;
                }
                let start = Instant::now();
                let (actions, shown, bar, slot) = (actions.clone(), shown.clone(), bar.clone(), held.clone());
                let id = glib::timeout_add_local(Duration::from_millis(16), move || {
                    let progress = start.elapsed().as_secs_f64() / HOLD.as_secs_f64();
                    bar.set_fraction(progress.min(1.0));
                    if progress < 1.0 {
                        return glib::ControlFlow::Continue;
                    }
                    slot.borrow_mut().take(); // finished: nothing for release to cancel
                    bar.set_fraction(0.0);
                    let _ = actions.send_blocking(Action::Answer(shown.borrow().clone(), Decision::Allow));
                    glib::ControlFlow::Break
                });
                if let Some(old) = held.replace(Some(id)) {
                    old.remove();
                }
            }
        });
        // Letting go early cancels.
        let cancel_hold = {
            let (bar, held) = (hold_bar.clone(), held.clone());
            move || {
                if let Some(id) = held.borrow_mut().take() {
                    id.remove();
                }
                bar.set_fraction(0.0);
            }
        };
        press.connect_drag_update({
            let cancel = cancel_hold.clone();
            move |_, dx, dy| {
                if dx.hypot(dy) > HOLD_SLOP {
                    cancel();
                }
            }
        });
        press.connect_drag_end({
            let cancel = cancel_hold.clone();
            move |_, _, _| cancel()
        });
        press.connect_cancel(move |_, _| cancel_hold());
        hold.add_controller(press);

        let filling = Rc::new(Cell::new(false));
        mode_picker.connect_selected_notify({
            let (actions, filling) = (actions.clone(), filling.clone());
            move |picker| {
                let Some(&(mode, _, description)) = MODES.get(picker.selected() as usize) else { return };
                picker.set_tooltip_text(Some(&format!("Permission mode: {description}")));
                if !filling.get() {
                    let _ = actions.send_blocking(Action::SetMode(mode.to_string()));
                }
            }
        });

        let models: Rc<RefCell<Vec<ModelInfo>>> = Rc::default();
        let efforts: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        model_picker.connect_selected_notify({
            let (actions, models, filling) = (actions.clone(), models.clone(), filling.clone());
            move |picker| {
                let models = models.borrow();
                let Some(m) = models.get(picker.selected() as usize) else { return };
                picker.set_tooltip_text(Some(&m.description));
                if !filling.get() {
                    let _ = actions.send_blocking(Action::SetModel(m.value.clone()));
                }
            }
        });
        effort_picker.connect_selected_notify({
            let (actions, efforts, filling) = (actions.clone(), efforts.clone(), filling.clone());
            move |picker| {
                let Some(level) = efforts.borrow().get(picker.selected() as usize).cloned() else { return };
                if !filling.get() {
                    let _ = actions.send_blocking(Action::SetEffort(level));
                }
            }
        });
        entry.connect_activate({
            let actions = actions.clone();
            move |entry| {
                // Empty text is fine when screenshots are attached; the app decides.
                let text = entry.text().trim().to_string();
                entry.set_text("");
                let _ = actions.send_blocking(Action::Submit(text));
            }
        });
        entry.connect_icon_press({
            let actions = actions.clone();
            move |_, _| {
                let _ = actions.send_blocking(Action::Screenshot);
            }
        });

        // Ctrl+V with an image on the clipboard attaches it instead of pasting text.
        // Slash commands: a list above the prompt box while typing "/name".
        let commands: Rc<RefCell<Vec<SlashCommand>>> = Rc::default();
        let command_list = ListBox::new();
        command_list.add_css_class("commands");
        let command_scroller = ScrolledWindow::new();
        command_scroller.set_hscrollbar_policy(PolicyType::Never);
        command_scroller.set_propagate_natural_height(true);
        command_scroller.set_max_content_height(220);
        // The rows ellipsize, so the list needs an explicit width (a minimum
        // content width is ignored without horizontal scrolling).
        command_list.set_size_request(WIDTH - 40, -1);
        command_scroller.set_child(Some(&command_list));
        let command_popup = Popover::new();
        command_popup.set_child(Some(&command_scroller));
        command_popup.set_parent(&entry);
        command_popup.set_position(gtk4::PositionType::Top);
        command_popup.set_has_arrow(false);
        command_popup.set_autohide(false);
        command_popup.set_can_focus(false);
        let complete = {
            let (entry, popup) = (entry.clone(), command_popup.clone());
            move |name: &str| {
                entry.set_text(&format!("/{name} "));
                entry.set_position(-1);
                popup.popdown();
                entry.grab_focus();
            }
        };
        entry.connect_changed({
            let (commands, list, popup) = (commands.clone(), command_list.clone(), command_popup.clone());
            move |entry| {
                let text = entry.text();
                let typed = text.strip_prefix('/').filter(|t| !t.contains(' '));
                let Some(typed) = typed else {
                    popup.popdown();
                    return;
                };
                while let Some(row) = list.first_child() {
                    list.remove(&row);
                }
                let typed = typed.to_lowercase();
                let mut matches: Vec<SlashCommand> = commands
                    .borrow()
                    .iter()
                    .filter(|c| c.name.to_lowercase().contains(&typed))
                    .cloned()
                    .collect();
                // Names starting with what was typed first, then alphabetical.
                matches.sort_by_key(|c| (!c.name.to_lowercase().starts_with(&typed), c.name.to_lowercase()));
                matches.truncate(30);
                for c in &matches {
                    let row = Label::new(None);
                    row.set_xalign(0.0);
                    row.set_ellipsize(pango::EllipsizeMode::End);
                    row.set_markup(&format!(
                        "<b>/{}</b> <span foreground=\"#9b978e\">{}  {}</span>",
                        glib::markup_escape_text(&c.name),
                        glib::markup_escape_text(&c.hint),
                        glib::markup_escape_text(&c.description)
                    ));
                    row.set_tooltip_text(Some(&c.description));
                    row.set_widget_name(&c.name);
                    list.append(&row);
                }
                if matches.is_empty() {
                    popup.popdown();
                } else {
                    list.select_row(list.row_at_index(0).as_ref());
                    popup.popup();
                }
            }
        });
        command_list.connect_row_activated({
            let complete = complete.clone();
            move |_, row| {
                if let Some(child) = row.child() {
                    complete(&child.widget_name());
                }
            }
        });
        // Arrow keys move through the list; Tab or Enter completes; Esc closes.
        let command_keys = EventControllerKey::new();
        command_keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        command_keys.connect_key_pressed({
            let (list, popup) = (command_list.clone(), command_popup.clone());
            move |_, key, _, _| {
                if !popup.is_visible() {
                    return glib::Propagation::Proceed;
                }
                let selected = list.selected_row().map_or(0, |r| r.index());
                let pick = |i: i32| {
                    if let Some(row) = list.row_at_index(i) {
                        list.select_row(Some(&row));
                        row.grab_focus();
                    }
                };
                match key {
                    gdk::Key::Down => pick(selected + 1),
                    gdk::Key::Up => pick((selected - 1).max(0)),
                    gdk::Key::Tab | gdk::Key::Return | gdk::Key::KP_Enter => {
                        if let Some(child) = list.selected_row().and_then(|r| r.child()) {
                            complete(&child.widget_name());
                        }
                    }
                    gdk::Key::Escape => popup.popdown(),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            }
        });
        entry.add_controller(command_keys);

        let paste = EventControllerKey::new();
        paste.set_propagation_phase(gtk4::PropagationPhase::Capture);
        paste.connect_key_pressed({
            let actions = actions.clone();
            move |_, key, _, mods| {
                if !(mods.contains(gdk::ModifierType::CONTROL_MASK) && key.to_lower() == gdk::Key::v) {
                    return glib::Propagation::Proceed;
                }
                let Some(clipboard) = gdk::Display::default().map(|d| d.clipboard()) else {
                    return glib::Propagation::Proceed;
                };
                if !clipboard.formats().contains_type(gdk::Texture::static_type()) {
                    return glib::Propagation::Proceed;
                }
                let actions = actions.clone();
                clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| {
                    if let Ok(Some(texture)) = result {
                        let _ = actions.send_blocking(Action::AttachImage(texture.save_to_png_bytes()));
                    }
                });
                glib::Propagation::Stop
            }
        });
        entry.add_controller(paste);

        Card {
            root,
            stack,
            title,
            status,
            stop,
            open_in_app,
            folder,
            folder_label,
            project,
            session,
            transcript,
            scroller,
            entry,
            current: RefCell::new(None),
            model_picker,
            models,
            effort_picker,
            efforts,
            filling,
            attachments,
            actions,
            perm_count,
            perm_heading,
            perm_detail,
            perm_note,
            always,
            deny,
            shown_request,
            shown_at,
            allow,
            tasks,
            meter,
            meter_label,
            meter_pie,
            meter_fill,
            commands,
            plan,
            hold,
            hold_bar,
            mode_picker,
        }
    }

    pub fn set_title(&self, title: &str) {
        self.title.set_text(title);
    }

    /// The conversation in progress (so the menu doesn't offer to resume it).
    pub fn set_session(&self, id: Option<&str>) {
        *self.session.borrow_mut() = id.map(str::to_string);
    }

    /// Shows the project folder Claude works in.
    pub fn set_project(&self, path: &Path) {
        self.folder_label.set_text(&projects::short_name(path));
        self.folder.set_tooltip_text(Some(&format!("Working in {}\nClick to change", projects::display(path))));
        *self.project.borrow_mut() = path.to_path_buf();
    }

    pub fn set_status(&self, status: &str, running: bool) {
        self.status.set_text(status);
        self.stop.set_visible(running);
    }

    /// The hand-off button only works once there's a conversation and Claude
    /// isn't in the middle of a reply.
    pub fn set_can_continue_in_app(&self, can: bool) {
        self.open_in_app.set_sensitive(can);
    }

    /// Fills the model picker with Claude Code's list and selects `selected`.
    pub fn set_models(&self, models: &[ModelInfo], selected: &str) {
        self.filling.set(true);
        *self.models.borrow_mut() = models.to_vec();
        let names: Vec<&str> = models.iter().map(|m| m.display_name.as_str()).collect();
        self.model_picker.set_model(Some(&StringList::new(&names)));
        let index = models.iter().position(|m| m.value == selected).unwrap_or(0);
        self.model_picker.set_selected(index as u32);
        self.model_picker.set_sensitive(!models.is_empty());
        self.filling.set(false);
    }

    /// Fills the effort picker with the current model's levels (plus "Auto")
    /// and selects `selected`. Hidden for models without effort levels.
    pub fn set_efforts(&self, levels: &[String], selected: Option<&str>) {
        self.filling.set(true);
        let mut options = vec![None];
        options.extend(levels.iter().cloned().map(Some));
        let names: Vec<String> = options.iter().map(|l| format!("{} effort", effort_label(l.as_deref()))).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.effort_picker.set_model(Some(&StringList::new(&names)));
        let index = options.iter().position(|l| l.as_deref() == selected).unwrap_or(0);
        self.effort_picker.set_selected(index as u32);
        self.effort_picker.set_visible(!levels.is_empty());
        *self.efforts.borrow_mut() = options;
        self.filling.set(false);
    }

    /// Shows the screenshots waiting to be sent, each with a remove button.
    pub fn set_attachments(&self, textures: &[gdk::Texture]) {
        while let Some(child) = self.attachments.first_child() {
            self.attachments.remove(&child);
        }
        for (i, texture) in textures.iter().enumerate() {
            let picture = Picture::for_paintable(texture);
            picture.set_overflow(Overflow::Hidden);
            picture.add_css_class("thumb");
            let remove = Button::with_label("✕");
            remove.add_css_class("thumb-remove");
            remove.set_halign(Align::End);
            remove.set_valign(Align::Start);
            remove.set_tooltip_text(Some("Remove"));
            let actions = self.actions.clone();
            remove.connect_clicked(move |_| {
                let _ = actions.send_blocking(Action::RemoveAttachment(i));
            });
            let thumb = gtk4::Overlay::new();
            thumb.set_child(Some(&picture));
            thumb.add_overlay(&remove);
            self.attachments.append(&thumb);
        }
        self.attachments.set_visible(!textures.is_empty());
    }

    /// Adds text to the draft (e.g. paths of dropped files).
    pub fn insert_text(&self, text: &str) {
        let current = self.entry.text();
        let joined = if current.is_empty() || current.ends_with(' ') { format!("{current}{text} ") } else { format!("{current} {text} ") };
        self.entry.set_text(&joined);
        self.entry.set_position(-1);
    }

    /// Puts an unsent message back in the prompt box (unless something new
    /// has been typed since).
    pub fn restore_draft(&self, text: &str) {
        if self.entry.text().is_empty() {
            self.entry.set_text(text);
            self.entry.set_position(-1);
        }
    }

    /// Empties the transcript (for a new session).
    pub fn clear(&self) {
        while let Some(child) = self.transcript.first_child() {
            self.transcript.remove(&child);
        }
        self.current.replace(None);
        self.scroller.set_visible(false);
        self.set_tasks(&[]);
        self.show_chat();
    }

    /// Your message in the transcript, with previews of any screenshots
    /// (preview texture, size sent to Claude).
    pub fn add_user(&self, text: &str, images: &[(gdk::Texture, (i32, i32))]) {
        let message = gtk4::Box::new(Orientation::Vertical, 4);
        message.set_halign(Align::End);
        for (texture, (w, h)) in images {
            let picture = Picture::for_paintable(texture);
            picture.set_overflow(Overflow::Hidden);
            picture.add_css_class("shot");
            picture.set_tooltip_text(Some(&format!("Screenshot · {w}×{h}")));
            // In a vertical box a picture is given the full row width and
            // scales up to keep its aspect ratio; a horizontal box keeps it at
            // its own (preview) size.
            let row = gtk4::Box::new(Orientation::Horizontal, 0);
            row.set_halign(Align::End);
            row.append(&picture);
            message.append(&row);
        }
        if !text.is_empty() {
            message.append(&message_label(text, "user"));
        }
        self.finish_reply();
        self.transcript.append(&message);
        self.scroller.set_visible(true);
    }

    /// The next streamed text starts a new paragraph.
    pub fn begin_assistant(&self) {
        self.finish_reply();
    }

    /// Streaming text shows as one label; it's re-rendered with code blocks
    /// and Copy buttons once complete (see `finish_reply`).
    pub fn append_text(&self, delta: &str) {
        let project = self.project.borrow().clone();
        let mut current = self.current.borrow_mut();
        let (label, raw) = current.get_or_insert_with(|| (self.push_label("", "assistant"), String::new()));
        raw.push_str(delta);
        label.set_markup(&markdown::to_pango(raw.trim(), Some(&project)));
    }

    pub fn add_text(&self, text: &str) {
        self.finish_reply();
        let reply = self.rich_reply(text);
        self.transcript.append(&reply);
        self.scroller.set_visible(true);
    }

    /// Turns the streamed reply (if any) into its final form.
    pub fn finish_reply(&self) {
        let Some((label, raw)) = self.current.take() else { return };
        if !raw.contains("```") {
            return; // plain prose is already final
        }
        let reply = self.rich_reply(&raw);
        self.transcript.insert_child_after(&reply, Some(&label));
        self.transcript.remove(&label);
    }

    /// A reply with each fenced code block in its own box with a Copy button.
    fn rich_reply(&self, text: &str) -> gtk4::Box {
        let project = self.project.borrow().clone();
        let reply = gtk4::Box::new(Orientation::Vertical, 6);
        for block in markdown::blocks(text.trim()) {
            match block {
                markdown::Block::Text(text) => {
                    let label = message_label("", "assistant");
                    label.set_markup(&markdown::to_pango(&text, Some(&project)));
                    reply.append(&label);
                }
                markdown::Block::Code { lang, code } => reply.append(&code_block(&lang, &code)),
            }
        }
        reply
    }

    /// Shows Claude's task checklist (hidden when empty).
    pub fn set_tasks(&self, tasks: &[(String, String, Option<String>)]) {
        while let Some(child) = self.tasks.first_child() {
            self.tasks.remove(&child);
        }
        if tasks.is_empty() {
            self.tasks.set_visible(false);
            return;
        }
        let done = tasks.iter().filter(|(_, status, _)| status == "completed").count();
        let heading = Label::new(Some(&format!("Tasks · {done}/{} done", tasks.len())));
        heading.add_css_class("dim");
        heading.set_xalign(0.0);
        self.tasks.append(&heading);
        for (subject, status, active) in tasks {
            let row = Label::new(None);
            row.set_xalign(0.0);
            row.set_ellipsize(pango::EllipsizeMode::End);
            let subject = glib::markup_escape_text(subject);
            row.set_markup(&match status.as_str() {
                "completed" => format!("<span foreground=\"#7fbf8e\">✓</span> <span foreground=\"#8a867e\"><s>{subject}</s></span>"),
                "in_progress" => {
                    let doing = active.as_deref().map(glib::markup_escape_text).unwrap_or(subject);
                    format!("<span foreground=\"#d97757\">◐</span> <b>{doing}</b>")
                }
                _ => format!("<span foreground=\"#9b978e\">○</span> {subject}"),
            });
            self.tasks.append(&row);
        }
        self.tasks.set_visible(true);
    }

    /// The header circle: how full the conversation's context window is.
    /// (Plan limits have their own bars at the bottom.)
    pub fn set_context(&self, context: Option<(f64, u64, u64)>) {
        let Some((percent, used, max)) = context else {
            self.meter.set_visible(false);
            return;
        };
        let level = if percent >= 90.0 {
            2
        } else if percent >= 75.0 {
            1
        } else {
            0
        };
        self.meter_fill.set((percent / 100.0, level));
        self.meter_pie.queue_draw();
        self.meter_label.set_text(&format!("{percent:.0}%"));
        for class in ["warn", "full"] {
            self.meter.remove_css_class(class);
        }
        match level {
            1 => self.meter.add_css_class("warn"),
            2 => self.meter.add_css_class("full"),
            _ => {}
        }
        self.meter.set_tooltip_text(Some(&format!(
            "Context window: {}k of {}k tokens used ({percent:.0}%)",
            used / 1000,
            max / 1000
        )));
        self.meter.set_visible(true);
    }

    /// Shows the plan's usage windows (5-hour, weekly…) as small bars.
    pub fn set_plan_usage(&self, limits: &[PlanLimit]) {
        while let Some(child) = self.plan.first_child() {
            self.plan.remove(&child);
        }
        for limit in limits {
            let item = gtk4::Box::new(Orientation::Horizontal, 6);
            item.set_hexpand(true);
            let name = Label::new(Some(&limit.label));
            name.add_css_class("dim");
            let bar = LevelBar::for_interval(0.0, 100.0);
            bar.set_value(limit.percent.clamp(0.0, 100.0));
            bar.set_hexpand(true);
            bar.set_valign(Align::Center);
            // Our own colours instead of GTK's default level offsets.
            bar.remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_LOW));
            bar.remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_HIGH));
            bar.remove_offset_value(Some(gtk4::LEVEL_BAR_OFFSET_FULL));
            let level = match limit.severity.as_str() {
                "normal" | "" => "ok",
                "warning" => "warn",
                _ => "full",
            };
            item.add_css_class(level);
            let percent = Label::new(Some(&format!("{:.0}%", limit.percent)));
            percent.add_css_class("dim");
            let resets = limit.resets_at.as_deref().and_then(|t| glib::DateTime::from_iso8601(t, None).ok()).and_then(|t| t.to_local().ok());
            let when = resets.and_then(|t| {
                let soon = (t.to_unix() - glib::DateTime::now_local().ok()?.to_unix()) < 86_400;
                t.format(if soon { "%H:%M" } else { "%a %-d %b, %H:%M" }).ok()
            });
            item.set_tooltip_text(Some(&match when {
                Some(when) => format!("{} limit: {:.0}% used, resets {when}", limit.label, limit.percent),
                None => format!("{} limit: {:.0}% used", limit.label, limit.percent),
            }));
            item.append(&name);
            item.append(&bar);
            item.append(&percent);
            self.plan.append(&item);
        }
        self.plan.set_visible(!limits.is_empty());
    }

    /// Slash commands and skills for the "/" menu.
    pub fn set_commands(&self, commands: Vec<SlashCommand>) {
        *self.commands.borrow_mut() = commands;
    }

    pub fn add_tool(&self, name: &str, summary: &str) {
        let summary: String = summary.lines().next().unwrap_or("").chars().take(120).collect();
        self.finish_reply();
        let label = self.push_label(&format!("▸ {name}  {summary}"), "tool");
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_wrap(false);
    }

    pub fn add_note(&self, text: &str, error: bool) {
        self.finish_reply();
        self.push_label(text, if error { "error" } else { "note" });
    }

    /// Shows the settings panel (built fresh by settings_page).
    pub fn show_settings(&self, page: &gtk4::Widget) {
        if let Some(old) = self.stack.child_by_name("settings") {
            self.stack.remove(&old);
        }
        self.stack.add_named(page, Some("settings"));
        self.root.remove_css_class("critical");
        self.stack.set_visible_child_name("settings");
    }

    pub fn settings_shown(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some("settings")
    }

    pub fn show_chat(&self) {
        self.root.remove_css_class("critical");
        self.stack.set_visible_child_name("chat");
    }

    /// Shows `mode` in the mode picker without treating it as a new choice.
    pub fn set_mode(&self, mode: &str) {
        let index = MODES.iter().position(|(m, _, _)| *m == mode).unwrap_or(0) as u32;
        if self.mode_picker.selected() != index {
            self.filling.set(true);
            self.mode_picker.set_selected(index);
            self.filling.set(false);
        }
    }

    /// `diff` is Pango markup of the change a file edit would make.
    pub fn show_permission(&self, req: &PermissionRequest, index: usize, total: usize, diff: Option<&str>) {
        // A different request restarts the guard; re-showing the same one
        // (e.g. after a screenshot) doesn't.
        if *self.shown_request.borrow() != req.request_id {
            self.shown_at.set(Instant::now());
        }
        *self.shown_request.borrow_mut() = req.request_id.clone();
        self.perm_count.set_text(&if total > 1 {
            format!("Permission · {} of {}", index + 1, total)
        } else {
            "Permission".to_string()
        });
        let verb = match req.tool_name.as_str() {
            "Bash" => "wants to run",
            "Edit" | "MultiEdit" | "NotebookEdit" => "wants to edit",
            "Write" => "wants to write",
            "Read" => "wants to read",
            "WebFetch" => "wants to fetch",
            "WebSearch" => "wants to search for",
            _ => "wants to use",
        };
        self.perm_heading.set_markup(&format!(
            "<b>{}</b> {verb}",
            glib::markup_escape_text(&req.display_name)
        ));
        let detail = match req.tool_name.as_str() {
            "Bash" | "Read" | "Write" | "Edit" | "MultiEdit" | "WebFetch" | "WebSearch" => {
                tool_summary(&req.tool_name, &req.input)
            }
            _ => serde_json::to_string_pretty(&req.input).unwrap_or_default(),
        };
        match diff {
            Some(markup) => self.perm_detail.set_markup(markup),
            None => self.perm_detail.set_text(&detail),
        }

        let note: Vec<&str> = [req.critical.as_deref(), req.description.as_deref(), req.decision_reason.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty() && *s != detail)
            .collect();
        self.perm_note.set_text(&note.join("\n"));
        self.perm_note.set_visible(!note.is_empty());

        // Critical actions: red card, no "Always", hold to allow.
        let critical = req.critical.is_some();
        if critical {
            self.root.add_css_class("critical");
            self.perm_count.set_text(&if total > 1 {
                format!("⚠ Critical action · {} of {}", index + 1, total)
            } else {
                "⚠ Critical action".to_string()
            });
        } else {
            self.root.remove_css_class("critical");
        }
        self.allow.set_visible(!critical);
        self.hold.set_visible(critical);
        self.hold_bar.set_visible(critical);
        self.hold_bar.set_fraction(0.0);

        let rule = req.always_rule().filter(|_| !critical);
        self.always.set_visible(rule.is_some());
        self.always.set_tooltip_text(rule.map(|r| format!("Don't ask again for {r}")).as_deref());

        self.stack.set_visible_child_name("permission");
        // If the card has the keyboard, focus sits on Deny so a stray Enter
        // never approves anything.
        self.deny.grab_focus();
    }

    fn push_label(&self, text: &str, class: &str) -> Label {
        let label = message_label(text, class);
        self.transcript.append(&label);
        self.scroller.set_visible(true);
        label
    }
}

/// Conversations in this folder, recent projects (other than the current
/// one), then folder actions.
fn fill_project_menu(menu: &gio::Menu, current: &Path, session: Option<&str>) {
    menu.remove_all();
    let conversations = gio::Menu::new();
    for past in projects::sessions(current, 9).into_iter().filter(|s| Some(s.id.as_str()) != session).take(8) {
        let label = format!("{} · {}", past.title, projects::ago(past.modified)).replace('_', "__");
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some("app.resume"), Some(&past.id.to_variant()));
        conversations.append_item(&item);
    }
    if conversations.n_items() > 0 {
        menu.append_section(Some("Conversations here"), &conversations);
    }
    let recent = gio::Menu::new();
    for path in projects::recent(8).into_iter().filter(|p| p != current) {
        // Menu labels treat "_" as a mnemonic marker; double it to show it.
        let label = projects::display(&path).replace('_', "__");
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some("app.set-cwd"), Some(&path.to_string_lossy().to_variant()));
        recent.append_item(&item);
    }
    if recent.n_items() > 0 {
        menu.append_section(Some("Recent projects"), &recent);
    }
    let other = gio::Menu::new();
    other.append(Some("Choose folder…"), Some("app.choose-cwd"));
    other.append(Some("Open in file manager"), Some("app.open-cwd"));
    menu.append_section(None, &other);
}

/// A fenced code block: language, Copy button, and the code.
fn code_block(lang: &str, code: &str) -> gtk4::Box {
    let block = gtk4::Box::new(Orientation::Vertical, 2);
    block.add_css_class("codeblock");
    let top = gtk4::Box::new(Orientation::Horizontal, 4);
    let language = Label::new(Some(if lang.is_empty() { "code" } else { lang }));
    language.add_css_class("dim");
    language.set_hexpand(true);
    language.set_xalign(0.0);
    let copy = Button::with_label("Copy");
    copy.add_css_class("flat");
    copy.add_css_class("copy");
    copy.connect_clicked({
        let code = code.to_string();
        move |button| {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&code);
            }
            button.set_label("Copied");
            let button = button.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || button.set_label("Copy"));
        }
    });
    top.append(&language);
    top.append(&copy);
    let text = Label::new(Some(code));
    text.add_css_class("codetext");
    text.set_xalign(0.0);
    text.set_wrap(true);
    text.set_wrap_mode(pango::WrapMode::WordChar);
    text.set_selectable(true);
    text.set_can_focus(false);
    block.append(&top);
    block.append(&text);
    block
}

/// A small pie for the context meter: `fill` is (fraction, level).
fn draw_pie(cr: &gtk4::cairo::Context, w: f64, h: f64, (fraction, level): (f64, u8)) {
    use std::f64::consts::PI;
    let (cx, cy, r) = (w / 2.0, h / 2.0, w.min(h) / 2.0 - 0.5);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    let _ = cr.fill();
    let (red, green, blue) = match level {
        2 => (0.94, 0.45, 0.38),
        1 => (0.95, 0.70, 0.35),
        _ => (0.61, 0.59, 0.56),
    };
    cr.set_source_rgb(red, green, blue);
    cr.move_to(cx, cy);
    cr.arc(cx, cy, r, -PI / 2.0, -PI / 2.0 + 2.0 * PI * fraction.clamp(0.0, 1.0));
    cr.close_path();
    let _ = cr.fill();
}

fn message_label(text: &str, class: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class(class);
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_can_focus(false);
    if class == "user" {
        label.set_halign(Align::End);
    }
    label
}

pub fn effort_label(level: Option<&str>) -> String {
    match level {
        None => "Auto".into(),
        Some("xhigh") => "Extra high".into(),
        Some(l) => {
            let mut c = l.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}
