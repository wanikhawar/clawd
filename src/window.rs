//! Clawd's overlay window: a layer-shell surface that floats above everything.
//!
//! The window holds Clawd and, when open, the speech-bubble card. Clawd's
//! position is the top-left of the sprite in monitor coordinates; the window
//! is anchored to whichever edges keep Clawd fixed while the card grows. The
//! card opens above Clawd unless there's no room, and extends to the right
//! unless Clawd is near the right edge.

use crate::app::Action;
use crate::card::{self, Card};
use crate::hypr;
use crate::sprite::{self, Anim};
use crate::state;
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Align, Application, ApplicationWindow, DrawingArea, EventControllerKey, GestureClick,
    GestureDrag, Orientation, PopoverMenu, TickCallbackId,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Wandering speed, in sprite-sized pixels per second.
const WALK_SPEED: f64 = 70.0;
/// Height of the bubble's tail between the card and Clawd.
const TAIL: i32 = 12;
/// Movement (px) below which a press on Clawd counts as a click, not a drag.
const CLICK_SLOP: i32 = 3;
/// Must match `.card` background in style.css.
const CARD_BG: (f64, f64, f64) = (0.141, 0.137, 0.129);

type Pos = (i32, i32);

pub struct Overlay {
    window: ApplicationWindow,
    layout: gtk4::Box,
    area: DrawingArea,
    bubble: gtk4::Box,
    tail: DrawingArea,
    card: Rc<Card>,
    monitor: gdk::Monitor,
    pos: Cell<Pos>,
    /// (card below Clawd, card extends to the left) as last laid out.
    flip: Cell<(bool, bool)>,
    dragging: Cell<bool>,
    /// Hidden from the bar button; Clawd keeps running in the background.
    user_hidden: Cell<bool>,
    /// Hidden while a screenshot is taken.
    capturing: Cell<bool>,
    /// Hidden because a fullscreen app or gamer mode is on.
    auto_hidden: Cell<bool>,
    /// Last time anything happened (shared with the app); drives sleeping.
    activity: Rc<Cell<Instant>>,
    /// Size multiplier (settings).
    scale: Cell<f64>,
    /// Seconds idle before dozing off; 0 = never (settings).
    sleep_after: Cell<u64>,
    /// Wandering is on in settings…
    wander: Cell<bool>,
    /// …and right now nothing needs Clawd to stay put (the app decides).
    wander_allowed: Cell<bool>,
    /// Where Clawd is walking to, and whether that's a perch on a window.
    walk_to: Cell<Option<(Pos, bool)>>,
    /// Perched on the focused window (and following it).
    perched: Cell<bool>,
    /// No new wander before this.
    rest_until: Cell<Instant>,
}

impl Overlay {
    pub fn build(
        app: &Application,
        card: Rc<Card>,
        anim: Rc<Cell<Anim>>,
        activity: Rc<Cell<Instant>>,
        actions: async_channel::Sender<Action>,
    ) -> Rc<Overlay> {
        let window = ApplicationWindow::new(app);
        window.set_title(Some("Clawd"));
        window.set_decorated(false);
        window.set_resizable(false);

        let saved = state::load();
        let monitor = pick_monitor(saved.monitor.as_deref());

        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace(Some("clawd"));
        window.set_monitor(Some(&monitor));
        window.set_keyboard_mode(KeyboardMode::None);
        // Position relative to the monitor edge, ignoring bars' reserved space.
        window.set_exclusive_zone(-1);

        let area = DrawingArea::new();
        let settings = crate::state::load().settings;
        let scale = settings.scale.clamp(0.5, 3.0);
        area.set_content_width((sprite::WIDTH as f64 * scale).round() as i32);
        area.set_content_height((sprite::HEIGHT as f64 * scale).round() as i32);
        area.set_halign(Align::Start);

        let tail = DrawingArea::new();
        tail.set_content_height(TAIL);

        let bubble = gtk4::Box::new(Orientation::Vertical, 0);
        bubble.append(&card.root);
        bubble.append(&tail);
        bubble.set_visible(false);

        let layout = gtk4::Box::new(Orientation::Vertical, 0);
        layout.append(&bubble);
        layout.append(&area);
        window.set_child(Some(&layout));

        let overlay = Rc::new(Overlay {
            window,
            layout,
            area,
            bubble,
            tail,
            card,
            monitor,
            pos: Cell::new((saved.x, saved.y)),
            flip: Cell::new((false, false)),
            dragging: Cell::new(false),
            user_hidden: Cell::new(false),
            auto_hidden: Cell::new(false),
            capturing: Cell::new(false),
            activity,
            scale: Cell::new(scale),
            sleep_after: Cell::new(settings.sleep_after),
            wander: Cell::new(settings.wander),
            wander_allowed: Cell::new(false),
            walk_to: Cell::new(None),
            perched: Cell::new(false),
            rest_until: Cell::new(Instant::now() + Duration::from_secs(20)),
        });

        let tick = tick_counter(&overlay.area);
        // Each animation starts from its first frame when Clawd's state changes.
        let current = Cell::new((Anim::Idle, 0u64));
        overlay.area.set_draw_func({
            let overlay = Rc::downgrade(&overlay);
            move |_, cr, _, _| {
                let Some(o) = overlay.upgrade() else { return };
                let sleep_after = o.sleep_after.get();
                let wanted = match anim.get() {
                    _ if o.dragging.get() => Anim::Carried,
                    Anim::Idle if o.walk_to.get().is_some() => Anim::Walk,
                    Anim::Idle if sleep_after > 0 && o.activity.get().elapsed().as_secs() > sleep_after => Anim::Sleep,
                    a => a,
                };
                let (shown, since) = current.get();
                let elapsed = tick.get().wrapping_sub(since);
                let a = if wanted.at_laptop() || matches!(wanted, Anim::Ask | Anim::Carried | Anim::Listen) {
                    wanted
                } else if shown.at_laptop() || (shown == Anim::PackUp && elapsed < sprite::PACK_UP) {
                    // The laptop goes away before Clawd does anything else.
                    Anim::PackUp
                } else {
                    wanted
                };
                // Moving between laptop states keeps Clawd seated; anything
                // else starts its animation from the first frame.
                if shown != a && !(shown.at_laptop() && a.at_laptop()) {
                    current.set((a, tick.get()));
                } else {
                    current.set((a, since));
                }
                let t = tick.get().wrapping_sub(current.get().1);
                cr.scale(o.scale.get(), o.scale.get());
                sprite::draw(cr, &sprite::frame(a, t));
            }
        });

        overlay.tail.set_draw_func({
            let overlay = Rc::downgrade(&overlay);
            move |_, cr, w, h| {
                let Some(o) = overlay.upgrade() else { return };
                let (below, left) = o.flip.get();
                let half = o.width() as f64 / 2.0;
                let cx = if left { w as f64 - half } else { half };
                let (base, tip) = if below { (h as f64, 0.0) } else { (0.0, h as f64) };
                cr.set_source_rgb(CARD_BG.0, CARD_BG.1, CARD_BG.2);
                cr.move_to(cx - 10.0, base);
                cr.line_to(cx + 10.0, base);
                cr.line_to(cx - 2.0, tip);
                cr.close_path();
                let _ = cr.fill();
            }
        });

        let card_click = GestureClick::new();
        card_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        card_click.connect_pressed({
            let overlay = Rc::downgrade(&overlay);
            move |_, _, _, _| {
                if let Some(o) = overlay.upgrade() {
                    o.window.set_keyboard_mode(KeyboardMode::OnDemand);
                }
            }
        });
        overlay.card.root.add_controller(card_click);

        // Files dropped on Clawd or the card: images attach, others are mentioned.
        for widget in [overlay.area.clone().upcast::<gtk4::Widget>(), overlay.card.root.clone().upcast()] {
            let drop = gtk4::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
            let actions = actions.clone();
            drop.connect_drop(move |_, value, _, _| {
                let Ok(files) = value.get::<gdk::FileList>() else { return false };
                let paths: Vec<_> = files.files().iter().filter_map(|f| f.path()).collect();
                !paths.is_empty() && actions.send_blocking(Action::DropFiles(paths)).is_ok()
            });
            widget.add_controller(drop);
        }

        overlay.area.add_controller(drag_controller(&overlay, actions.clone()));
        overlay.area.add_controller(menu_controller(&overlay.area));

        let keys = EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                let _ = actions.send_blocking(Action::Hide);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        overlay.window.add_controller(keys);

        // The saved spot may be off this monitor (another resolution, or a
        // monitor that's no longer connected).
        overlay.pos.set(overlay.clamp(overlay.pos.get()));
        overlay.place();
        overlay.window.present();

        glib::timeout_add_local(Duration::from_millis(33), {
            let overlay = Rc::downgrade(&overlay);
            move || {
                let Some(o) = overlay.upgrade() else { return glib::ControlFlow::Break };
                o.wander_step();
                glib::ControlFlow::Continue
            }
        });
        overlay
    }

    pub fn card_shown(&self) -> bool {
        self.bubble.is_visible()
    }

    /// Shows or hides the card. `take_keyboard` is for when the user opened it:
    /// when Clawd opens it on its own (a permission prompt) it must not steal
    /// the keyboard from whatever the user is typing into; clicking inside the
    /// card takes it then.
    pub fn show_card(&self, show: bool, take_keyboard: bool) {
        if show != self.card_shown() {
            self.bubble.set_visible(show);
            self.place();
        }
        if !show {
            self.window.set_keyboard_mode(KeyboardMode::None);
        } else if take_keyboard {
            self.take_keyboard();
        }
    }

    /// Clawd's on-screen size at the current scale.
    fn width(&self) -> i32 {
        (sprite::WIDTH as f64 * self.scale.get()).round() as i32
    }

    fn height(&self) -> i32 {
        (sprite::HEIGHT as f64 * self.scale.get()).round() as i32
    }

    /// Applies the settings that affect Clawd itself.
    pub fn apply_settings(&self, settings: &crate::state::Settings) {
        let scale = settings.scale.clamp(0.5, 3.0);
        if (scale - self.scale.get()).abs() > f64::EPSILON {
            self.scale.set(scale);
            self.area.set_content_width(self.width());
            self.area.set_content_height(self.height());
            self.pos.set(self.clamp(self.pos.get()));
            self.place();
        }
        self.sleep_after.set(settings.sleep_after);
        self.wander.set(settings.wander);
        if !settings.wander {
            self.stop_wandering();
        }
    }

    /// Whether Clawd may wander right now (idle, nobody waiting on it).
    pub fn set_wander_allowed(&self, allowed: bool) {
        self.wander_allowed.set(allowed);
        if !allowed {
            self.stop_wandering();
        }
    }

    fn stop_wandering(&self) {
        if self.walk_to.take().is_some() {
            self.save();
        }
        self.perched.set(false);
    }

    /// One step of wandering (called ~30 times a second).
    fn wander_step(&self) {
        let free = self.wander.get()
            && self.wander_allowed.get()
            && !self.dragging.get()
            && !self.card_shown()
            && self.window.is_visible()
            && !self.asleep();
        if !free {
            if self.walk_to.get().is_some() {
                self.stop_wandering();
            }
            return;
        }
        match self.walk_to.get() {
            Some(((tx, ty), perch)) => {
                let (x, y) = self.pos.get();
                let (dx, dy) = ((tx - x) as f64, (ty - y) as f64);
                let distance = dx.hypot(dy);
                let step = WALK_SPEED * self.scale.get() / 30.0;
                if distance <= step {
                    self.move_to((tx, ty));
                    self.walk_to.set(None);
                    self.perched.set(perch);
                    self.save();
                    // Rest a while before the next stroll.
                    self.rest_until.set(Instant::now() + Duration::from_secs(6 + (coin() % 14) as u64));
                } else {
                    self.move_to((x + (dx / distance * step).round() as i32, y + (dy / distance * step).round() as i32));
                }
            }
            None if self.perched.get() => {
                // Follow the window it sits on (checked about once a second).
                if Instant::now() >= self.rest_until.get() {
                    self.rest_until.set(Instant::now() + Duration::from_secs(1));
                    match self.perch_spot(false) {
                        Some(spot) if spot != self.pos.get() => self.walk_to.set(Some((spot, true))),
                        Some(_) => {}
                        None => self.perched.set(false),
                    }
                }
            }
            None if Instant::now() >= self.rest_until.get() => {
                // Half the time, perch on the focused window; otherwise (or
                // if there's no room on it) stroll along the current height.
                let perch = if coin().is_multiple_of(2) { self.perch_spot(true) } else { None };
                let target = match perch {
                    Some(spot) => (spot, true),
                    None => {
                        let span = (self.geometry().width() - self.width()).max(1) as u32;
                        (((coin() % span) as i32, self.pos.get().1), false)
                    }
                };
                self.walk_to.set(Some(target));
            }
            None => {}
        }
    }

    fn asleep(&self) -> bool {
        let after = self.sleep_after.get();
        after > 0 && self.activity.get().elapsed().as_secs() > after
    }

    /// A spot on top of the focused window (if it's on Clawd's monitor):
    /// `fresh` picks a new place along it; otherwise keep the same offset.
    fn perch_spot(&self, fresh: bool) -> Option<Pos> {
        let (wx, wy, ww, _) = hypr::active_window_geometry()?;
        let geo = self.geometry();
        let (wx, wy) = (wx as i32 - geo.x(), wy as i32 - geo.y());
        if wx + ww as i32 <= 0 || wx >= geo.width() || wy < self.height() {
            return None; // elsewhere, or no room above it
        }
        // Feet on the window's top edge (the sprite has empty space below).
        let y = wy - self.height() + (8.0 * self.scale.get()) as i32;
        let along = if fresh {
            (coin() % ((ww as i32 - self.width()).max(1) as u32)) as i32
        } else {
            (self.pos.get().0 - wx).clamp(0, (ww as i32 - self.width()).max(0))
        };
        Some(self.clamp((wx + along, y)))
    }

    pub fn auto_hidden(&self) -> bool {
        self.auto_hidden.get()
    }

    /// Steps aside for fullscreen apps and gamer mode.
    pub fn set_auto_hidden(&self, hidden: bool) {
        self.auto_hidden.set(hidden);
        self.apply_visibility();
    }

    /// Connector name of Clawd's monitor, e.g. "DP-2".
    pub fn monitor_name(&self) -> Option<String> {
        self.monitor.connector().map(|c| c.to_string())
    }

    pub fn application(&self) -> Option<gtk4::Application> {
        self.window.application()
    }

    pub fn user_hidden(&self) -> bool {
        self.user_hidden.get()
    }

    /// Hides or shows Clawd entirely (the bar button).
    pub fn set_user_hidden(&self, hidden: bool) {
        self.user_hidden.set(hidden);
        self.apply_visibility();
    }

    /// Keeps Clawd out of a screenshot being taken.
    pub fn set_capturing(&self, capturing: bool) {
        self.capturing.set(capturing);
        self.apply_visibility();
    }

    fn apply_visibility(&self) {
        let visible = !self.user_hidden.get() && !self.capturing.get() && !self.auto_hidden.get();
        if visible != self.window.is_visible() {
            self.window.set_visible(visible);
        }
    }

    fn take_keyboard(&self) {
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.card.entry.grab_focus();
    }

    fn geometry(&self) -> gdk::Rectangle {
        self.monitor.geometry()
    }

    /// Anchors and offsets the window so Clawd sits at `pos`.
    fn place(&self) {
        let geo = self.geometry();
        let (x, y) = self.pos.get();

        let below = y < card::MAX_HEIGHT + TAIL;
        let left = x + card::WIDTH > geo.width();
        if (below, left) != self.flip.get() {
            self.flip.set((below, left));
            if below {
                self.layout.reorder_child_after(&self.bubble, Some(&self.area));
                self.bubble.reorder_child_after(&self.card.root, Some(&self.tail));
            } else {
                self.layout.reorder_child_after(&self.area, Some(&self.bubble));
                self.bubble.reorder_child_after(&self.tail, Some(&self.card.root));
            }
            self.area.set_halign(if left { Align::End } else { Align::Start });
            self.tail.queue_draw();
        }

        let w = &self.window;
        w.set_anchor(Edge::Top, below);
        w.set_anchor(Edge::Bottom, !below);
        w.set_anchor(Edge::Left, !left);
        w.set_anchor(Edge::Right, left);
        LayerShell::set_margin(w, Edge::Top, y);
        LayerShell::set_margin(w, Edge::Bottom, geo.height() - y - self.height());
        LayerShell::set_margin(w, Edge::Left, x);
        LayerShell::set_margin(w, Edge::Right, geo.width() - x - self.width());
    }

    /// Keeps a position fully on Clawd's monitor.
    fn clamp(&self, (x, y): Pos) -> Pos {
        let geo = self.geometry();
        (
            x.clamp(0, (geo.width() - self.width()).max(0)),
            y.clamp(0, (geo.height() - self.height()).max(0)),
        )
    }

    /// Moves Clawd, keeping it fully on its monitor.
    fn move_to(&self, p: Pos) {
        let p = self.clamp(p);
        if p != self.pos.get() {
            self.pos.set(p);
            self.place();
        }
    }

    fn save(&self) {
        let (x, y) = self.pos.get();
        let monitor = self.monitor.connector().map(|c| c.to_string());
        state::update(|s| {
            s.x = x;
            s.y = y;
            s.monitor = monitor;
        });
    }
}

/// A cheap random number for wandering decisions.
fn coin() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_micros())
}

/// Advances an animation tick and redraws `area` sprite::FPS times per second.
fn tick_counter(area: &DrawingArea) -> Rc<Cell<u64>> {
    let tick = Rc::new(Cell::new(0u64));
    glib::timeout_add_local(Duration::from_millis(1000 / sprite::FPS), {
        let (area, tick) = (area.downgrade(), tick.clone());
        move || {
            let Some(area) = area.upgrade() else { return glib::ControlFlow::Break };
            tick.set(tick.get().wrapping_add(1));
            area.queue_draw();
            glib::ControlFlow::Continue
        }
    });
    tick
}

/// Left-drag moves Clawd; a press without movement is a click.
///
/// The window's position is computed from the cursor's absolute position
/// (via Hyprland IPC) minus the point where Clawd was grabbed. Pointer events
/// arrive in window coordinates, and the window itself is moving, so summing
/// their offsets double-counts movement whenever the compositor lags a frame
/// behind; that showed up as overshoot on release.
///
/// The cursor is sampled once per frame on the frame clock rather than on
/// every motion event: high-rate mice deliver several events per frame, and
/// handling each one (an IPC round trip plus a surface commit) queues up work.
fn drag_controller(overlay: &Rc<Overlay>, actions: async_channel::Sender<Action>) -> GestureDrag {
    let drag = GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);

    // Where inside Clawd the drag started. Accurate because nothing has moved yet.
    let grab = Rc::new(Cell::new((0.0, 0.0)));
    // Clawd's position when the press started, to tell clicks from drags.
    let start = Rc::new(Cell::new((0, 0)));
    let ticker: Rc<RefCell<Option<TickCallbackId>>> = Rc::default();

    let moved = {
        let start = start.clone();
        move |o: &Overlay| {
            let ((x, y), (sx, sy)) = (o.pos.get(), start.get());
            (x - sx).abs() > CLICK_SLOP || (y - sy).abs() > CLICK_SLOP
        }
    };

    // Moves Clawd under the cursor via Hyprland.
    let follow_cursor = Rc::new({
        let (overlay, grab, moved) = (Rc::downgrade(overlay), grab.clone(), moved.clone());
        move || {
            let (Some(o), Some((cx, cy))) = (overlay.upgrade(), hypr::cursor_pos()) else { return };
            let geo = o.geometry();
            let (gx, gy) = grab.get();
            o.move_to((
                (cx - geo.x() as f64 - gx).round() as i32,
                (cy - geo.y() as f64 - gy).round() as i32,
            ));
            o.dragging.set(moved(&o));
        }
    });

    drag.connect_drag_begin({
        let (overlay, ticker, follow_cursor) = (Rc::downgrade(overlay), ticker.clone(), follow_cursor.clone());
        move |_, x, y| {
            let Some(o) = overlay.upgrade() else { return };
            o.activity.set(Instant::now()); // picking Clawd up wakes it
            grab.set((x, y));
            start.set(o.pos.get());
            if hypr::cursor_pos().is_some() {
                let follow_cursor = follow_cursor.clone();
                let id = o.area.add_tick_callback(move |_, _| {
                    follow_cursor();
                    glib::ControlFlow::Continue
                });
                ticker.replace(Some(id));
            }
        }
    });

    // Fallback when not on Hyprland. GTK's offsets are relative to Clawd's
    // own surface, which moves with each step: once Clawd has caught up with
    // the pointer the offset is back near zero. So the offset is added to the
    // current position, not the starting one (that would snap Clawd back).
    // It can drift if an event arrives before the previous move took effect.
    drag.connect_drag_update({
        let (overlay, ticker, moved) = (Rc::downgrade(overlay), ticker.clone(), moved.clone());
        move |_, dx, dy| {
            let Some(o) = overlay.upgrade() else { return };
            if ticker.borrow().is_none() {
                let (x, y) = o.pos.get();
                o.move_to((x + dx.round() as i32, y + dy.round() as i32));
                o.dragging.set(moved(&o));
            }
        }
    });

    drag.connect_drag_end({
        let overlay = Rc::downgrade(overlay);
        move |_, _, _| {
            let Some(o) = overlay.upgrade() else { return };
            if let Some(id) = ticker.take() {
                id.remove();
                // Land exactly under the cursor where the button was released.
                follow_cursor();
            }
            o.dragging.set(false);
            if moved(&o) {
                o.save();
            } else {
                let _ = actions.send_blocking(Action::ToggleCard);
            }
        }
    });

    drag
}

/// Right-click opens Clawd's menu.
fn menu_controller(area: &DrawingArea) -> GestureClick {
    let menu = gio::Menu::new();
    let session = gio::Menu::new();
    session.append(Some("New session"), Some("app.new-session"));
    session.append(Some("End session"), Some("app.end-session"));
    menu.append_section(None, &session);
    menu.append(Some("Screenshot a region"), Some("app.screenshot"));
    menu.append(Some("Continue in Claude app"), Some("app.continue-in-app"));
    let app_section = gio::Menu::new();
    app_section.append(Some("Settings…"), Some("app.settings"));
    app_section.append(Some("Hide Clawd"), Some("app.toggle"));
    app_section.append(Some("Quit Clawd"), Some("app.quit"));
    menu.append_section(None, &app_section);
    let popover = PopoverMenu::from_model(Some(&menu));
    popover.set_parent(area);
    popover.set_has_arrow(false);

    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.connect_pressed(move |_, _, x, y| {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    });
    click
}

/// The saved monitor if it's still connected, else the one under the cursor,
/// else the first.
fn pick_monitor(saved: Option<&str>) -> gdk::Monitor {
    let display = gdk::Display::default().expect("no display");
    let monitors: Vec<gdk::Monitor> = display
        .monitors()
        .iter::<gdk::Monitor>()
        .filter_map(Result::ok)
        .collect();
    let by_name = saved.and_then(|name| monitors.iter().find(|m| m.connector().as_deref() == Some(name)));
    let under_cursor = || {
        let (cx, cy) = hypr::cursor_pos()?;
        monitors.iter().find(|m| {
            let g = m.geometry();
            (g.x() as f64..(g.x() + g.width()) as f64).contains(&cx)
                && (g.y() as f64..(g.y() + g.height()) as f64).contains(&cy)
        })
    };
    by_name
        .or_else(under_cursor)
        .or(monitors.first())
        .cloned()
        .expect("no monitors")
}
