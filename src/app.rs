//! Glue between the UI and the Claude session.
//!
//! Everything that happens — a button click, a line from Claude — arrives as
//! a `Msg` on one channel and is handled here, one at a time, on the GTK
//! main thread.

use crate::card::{effort_label, Card, MODES};
use crate::bar::{self, BarState};
use crate::critical;
use crate::claude::protocol::{self, Decision, Event, Image, ModelInfo, PermissionRequest};
use crate::image::{self, Attachment};
use crate::projects;
use crate::claude::session::Session;
use crate::sprite::{Anim, Tool};
use crate::state;
use crate::window::Overlay;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Something the user did.
#[derive(Debug, Clone)]
pub enum Action {
    Submit(String),
    Stop,
    /// An answer to the permission request with this ID.
    Answer(String, Decision),
    Hide,
    /// Show Clawd with the card open (launching from an app launcher).
    Show,
    /// Hide or show Clawd entirely (the bar button). Hidden, it keeps running.
    Toggle,
    /// Clicked Clawd: open or close the card.
    ToggleCard,
    /// Hand the conversation over to the Claude desktop app.
    ContinueInApp,
    /// Work in another project folder (starts a new session).
    SetCwd(PathBuf),
    /// Continue a past conversation in this folder.
    Resume(String),
    /// Chat without a project (one-off tasks and questions).
    NoProject,
    /// Pick the project folder with a folder chooser.
    ChooseCwd,
    /// Show the project folder in the file manager.
    OpenCwd,
    SetModel(String),
    /// A permission mode from card::MODES.
    SetMode(String),
    /// `None` = the model's default effort.
    SetEffort(Option<String>),
    NewSession,
    EndSession,
    /// Select a screen region to attach.
    Screenshot,
    /// Capture the focused window and attach it.
    ScreenshotWindow,
    /// Open the quick-ask box at the cursor.
    QuickAsk,
    /// Quick ask about the text selected anywhere.
    Explain,
    /// Files dropped on Clawd or its card.
    DropFiles(Vec<PathBuf>),
    /// A message from the quick-ask box: send it and show the answer.
    QuickSubmit(String),
    OpenSettings,
    CloseSettings,
    UpdateSettings(crate::state::Settings),
    /// Open critical.toml in the default text editor.
    EditCriticalRules,
    /// Start or stop voice input.
    Voice,
    /// An image pasted from the clipboard (PNG bytes).
    AttachImage(glib::Bytes),
    RemoveAttachment(usize),
    /// Ask the newest prompt again for a new reply.
    Regenerate,
}

/// A prompt, kept so its reply can be regenerated.
#[derive(Clone)]
struct Turn {
    /// The conversation entry the prompt was saved as.
    uuid: String,
    /// The newest entry before it (`None`: it started the conversation).
    after: Option<String>,
    text: String,
    images: Vec<Image>,
}

pub enum Msg {
    Ui(Action),
    /// An event from the session with the given generation. Events from a
    /// session that has since been replaced are ignored.
    Claude(u64, Event),
    /// A region capture finished: PNG bytes, or `None` if cancelled.
    Captured(Result<Option<Vec<u8>>, String>),
    /// Re-evaluate Clawd's animation (a reaction wore off).
    Refresh,
    /// The selected text was read for "explain" (`None`: nothing selected).
    Selection(Option<String>),
    /// What you said, transcribed.
    Transcribed(Result<String, String>),
    /// The speech model finished downloading.
    ModelDownloaded(Result<PathBuf, String>),
    /// A recording hit its time limit (identified by when it started).
    VoiceTimeout(Instant),
    /// Nobody answered this critical action in time.
    CriticalTimeout(String),
    /// Windows, workspaces or gamer mode may have changed: re-check whether
    /// Clawd should step aside.
    Environment,
}

/// What Claude has written in the current turn, streamed or not.
#[derive(Default)]
struct ReplyText {
    /// The whole turn, for Copy.
    turn: String,
    /// The latest message, for the "finished" notification and reading aloud.
    last: String,
    /// The next text starts a new message (a paragraph break in `turn`).
    new_message: bool,
}

impl ReplyText {
    fn start_turn(&mut self) {
        *self = ReplyText::default();
    }

    /// A streamed message started. (Messages that aren't streamed don't
    /// announce themselves; they join the latest one.)
    fn start_message(&mut self) {
        self.last.clear();
        self.new_message = true;
    }

    fn add(&mut self, text: &str) {
        if std::mem::take(&mut self.new_message) && !self.turn.is_empty() {
            self.turn.push_str("\n\n");
        }
        self.turn.push_str(text);
        self.last.push_str(text);
    }
}

impl Msg {
    /// Whether this keeps Clawd awake. Clawd's own periodic checks don't.
    fn is_activity(&self) -> bool {
        !matches!(self, Msg::Refresh | Msg::Environment)
    }
}

pub struct App {
    overlay: Rc<Overlay>,
    card: Rc<Card>,
    anim: Rc<Cell<Anim>>,
    /// Last time anything happened; Clawd dozes off when this gets old.
    activity: Rc<Cell<Instant>>,
    /// What Claude is doing while a turn runs: Think, Write or Work.
    busy: Anim,
    /// A short reaction (Happy or Error) and when it wears off.
    reaction: Option<(Anim, Instant)>,
    tx: async_channel::Sender<Msg>,

    cwd: PathBuf,
    session: Option<Session>,
    generation: u64,
    /// Kept so a crashed or restarted process resumes the same conversation.
    session_id: Option<String>,
    running: bool,
    pending: VecDeque<PermissionRequest>,
    /// Selected model, as Claude Code names it ("opus", "claude-sonnet-4-6", …).
    model: String,
    /// The models offered in the picker (Claude Code's list minus its "Default" alias).
    models: Vec<ModelInfo>,
    /// Selected effort; `None` = the model's default.
    effort: Option<String>,
    /// Permission mode for this project (a value from card::MODES).
    mode: String,
    /// Screenshots waiting to go out with the next message.
    attachments: Vec<Attachment>,
    capturing: bool,
    /// The bar button's state as last written.
    bar: Option<(BarState, bool)>,
    /// Claude finished while you weren't looking; shown on the bar until
    /// you open the card.
    unread: bool,
    /// When the current turn started (long turns notify when they finish).
    turn_started: Option<Instant>,
    /// What Claude has written this turn.
    reply: ReplyText,
    /// The newest prompt, while its reply can be regenerated.
    last_turn: Option<Turn>,
    /// The newest entry in the conversation.
    last_entry: Option<String>,
    /// Set by Regenerate: the next start resumes the conversation at (entry
    /// to keep, prompt to drop).
    rewind: Option<(String, String)>,
    /// Claude's task checklist: (id, subject, status, active form).
    tasks: Vec<(String, String, String, Option<String>)>,
    /// Context window use: (percent, used tokens, max tokens).
    context: Option<(f64, u64, u64)>,
    settings: crate::state::Settings,
    /// Voice input being recorded.
    recording: Option<crate::voice::Recording>,
    /// Transcribing or downloading the speech model.
    voice_busy: bool,
    /// Reading a reply aloud.
    speaking: Option<std::process::Child>,
}

/// Turns at least this long send a notification when they finish while
/// Clawd isn't showing.
const NOTIFY_AFTER: Duration = Duration::from_secs(10);

/// Gamer mode (Noctalia's nomadcxx/gamer-mode plugin) is on while this exists.
fn gamer_mode_on() -> bool {
    dirs::home_dir()
        .map(|h| h.join(".local/state/noctalia/plugins/data/nomadcxx/gamer-mode/session.json"))
        .is_some_and(|p| p.exists())
}

const HAPPY_FOR: Duration = Duration::from_millis(3200);
const UPSET_FOR: Duration = Duration::from_millis(3500);

impl App {
    pub fn new(
        overlay: Rc<Overlay>,
        card: Rc<Card>,
        anim: Rc<Cell<Anim>>,
        activity: Rc<Cell<Instant>>,
        tx: async_channel::Sender<Msg>,
        cwd: PathBuf,
    ) -> App {
        card.set_title("Clawd");
        card.set_project(&cwd);
        card.set_status("No session", false);
        let saved = state::load();
        let model = saved.model.unwrap_or_else(|| "default".into());
        let mut app = App {
            overlay,
            card,
            anim,
            activity,
            busy: Anim::Think,
            reaction: None,
            tx,
            cwd,
            session: None,
            generation: 0,
            session_id: None,
            running: false,
            pending: VecDeque::new(),
            model,
            models: Vec::new(),
            effort: saved.effort,
            mode: String::new(), // loaded below
            attachments: Vec::new(),
            capturing: false,
            bar: None,
            unread: false,
            turn_started: None,
            reply: ReplyText::default(),
            last_turn: None,
            last_entry: None,
            rewind: None,
            tasks: Vec::new(),
            context: None,
            settings: saved.settings.clone(),
            recording: None,
            voice_busy: false,
            speaking: None,
        };
        app.overlay.apply_settings(&app.settings);
        // Make sure clawd.lua matches the settings (only writes on change).
        let settings = app.settings.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::hyprconf::apply(&settings) {
                eprintln!("clawd: Hyprland config: {e}");
            }
        });
        app.load_mode();
        app
    }

    pub fn handle(&mut self, msg: Msg) {
        if msg.is_activity() {
            self.activity.set(Instant::now());
        }
        match msg {
            Msg::Ui(action) => self.on_action(action),
            Msg::Claude(generation, event) if generation == self.generation => self.on_event(event),
            Msg::Claude(..) => {}
            Msg::Captured(result) => self.on_captured(result),
            Msg::Refresh => {}
            Msg::Selection(Some(text)) => self.quick_ask("Explain this", Some(text)),
            Msg::Transcribed(result) => {
                self.voice_busy = false;
                match result {
                    Ok(text) if !text.is_empty() => {
                        self.card.insert_text(&text);
                        self.card.set_status("Heard you: press Enter to send", false);
                        self.overlay.set_user_hidden(false);
                        self.show_current_view();
                        self.overlay.show_card(true, true);
                    }
                    Ok(_) => {
                        self.card.set_status("Ready", false);
                        self.voice_note("I didn't catch anything. Try again a bit closer to the mic.");
                    }
                    Err(e) => {
                        self.card.set_status("Ready", false);
                        self.voice_note(&format!("Couldn't transcribe: {e}"));
                    }
                }
            }
            Msg::ModelDownloaded(result) => {
                self.voice_busy = false;
                match result {
                    Ok(_) => self.voice_note("Speech model ready. Press the voice hotkey to talk."),
                    Err(e) => self.voice_note(&format!("Couldn't download the speech model: {e}")),
                }
            }
            Msg::VoiceTimeout(started) => {
                if self.recording.as_ref().is_some_and(|r| r.started == started) {
                    self.voice();
                }
            }
            Msg::Selection(None) => {
                if let Some(app) = self.overlay.application() {
                    let n = gtk4::gio::Notification::new("Nothing selected");
                    n.set_body(Some("Select some text first, then press the explain hotkey."));
                    app.send_notification(Some("clawd-explain"), &n);
                }
            }
            Msg::Environment => {
                let fullscreen = self.overlay.monitor_name().is_some_and(|m| crate::hypr::fullscreen_on(&m));
                self.overlay.set_auto_hidden(fullscreen || gamer_mode_on());
            }
            Msg::CriticalTimeout(request_id) => {
                if self.pending.iter().any(|r| r.request_id == request_id) {
                    self.answer(&request_id, Decision::Deny);
                    self.card.add_note("No answer in time, so the critical action was denied.", false);
                }
            }
        }
        self.card.set_can_continue_in_app(self.session_id.is_some() && !self.running);
        self.refresh_anim();
        self.refresh_bar();
    }

    fn on_action(&mut self, action: Action) {
        match action {
            Action::Submit(text) => self.submit(&text),
            Action::Stop => {
                if let Some(s) = &mut self.session
                    && let Err(e) = s.interrupt() {
                        self.card.add_note(&format!("Couldn't stop Claude: {e}"), true);
                    }
            }
            Action::Answer(request_id, decision) => self.answer(&request_id, decision),
            Action::Hide => self.overlay.show_card(false, false),
            Action::Toggle => self.overlay.set_user_hidden(!self.overlay.user_hidden()),
            Action::Show | Action::ToggleCard => {
                if matches!(action, Action::Show) {
                    self.overlay.set_user_hidden(false);
                }
                let show = matches!(action, Action::Show) || !self.overlay.card_shown();
                if show {
                    self.unread = false;
                    self.withdraw("clawd-done");
                }
                if show {
                    self.show_current_view();
                    // Start Claude now so the model list is ready; no prompt
                    // is sent, so this doesn't use any of your quota.
                    if let Err(e) = self.ensure_session() {
                        self.card.add_note(&format!("Couldn't reach Claude: {e}"), true);
                    }
                }
                self.overlay.show_card(show, true);
            }
            Action::ContinueInApp => self.continue_in_app(),
            Action::SetCwd(path) => self.set_cwd(path),
            Action::Resume(id) => self.resume(id),
            Action::ChooseCwd => self.choose_cwd(),
            Action::NoProject => self.set_cwd(projects::no_project()),
            Action::OpenCwd => {
                let uri = gtk4::gio::File::for_path(&self.cwd).uri();
                if let Err(e) = gtk4::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>) {
                    self.card.add_note(&format!("Couldn't open the folder: {e}"), true);
                }
            }
            Action::SetModel(model) => self.set_model(model),
            Action::SetMode(mode) => self.set_mode(mode),
            Action::SetEffort(level) => self.set_effort(level),
            Action::Screenshot => self.screenshot(None),
            Action::ScreenshotWindow => match crate::hypr::active_window_geometry() {
                Some(rect) => self.screenshot(Some(rect)),
                None => self.card.add_note("There's no focused window to capture.", true),
            },
            Action::QuickAsk => self.quick_ask("", None),
            Action::Explain => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send_blocking(Msg::Selection(image::selected_text()));
                });
            }
            Action::DropFiles(files) => self.drop_files(files),
            Action::OpenSettings => {
                let page = crate::settings_page::build(&self.settings, self.card_actions());
                self.card.show_settings(&page);
                self.overlay.set_user_hidden(false);
                self.overlay.show_card(true, true);
            }
            Action::CloseSettings => {
                self.card.show_chat();
                self.show_current_view();
            }
            Action::UpdateSettings(settings) => self.update_settings(settings),
            Action::EditCriticalRules => {
                critical::Rules::load(); // creates the file with defaults if missing
                if let Some(path) = dirs::config_dir().map(|d| d.join("clawd/critical.toml")) {
                    let uri = gtk4::gio::File::for_path(&path).uri();
                    if let Err(e) = gtk4::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>) {
                        self.card.add_note(&format!("Couldn't open {}: {e}", path.display()), true);
                    }
                }
            }
            Action::Voice => self.voice(),
            Action::QuickSubmit(message) => {
                self.overlay.set_user_hidden(false);
                self.show_current_view();
                self.overlay.show_card(true, false);
                self.submit(&message);
            }
            Action::AttachImage(bytes) => self.attach(&bytes),
            Action::RemoveAttachment(i) => {
                if i < self.attachments.len() {
                    self.attachments.remove(i);
                    self.show_attachments();
                }
            }
            Action::Regenerate => self.regenerate(),
            Action::NewSession => {
                self.end_session();
                self.card.clear();
                if let Err(e) = self.ensure_session() {
                    self.card.add_note(&format!("Couldn't reach Claude: {e}"), true);
                }
                self.overlay.show_card(true, true);
            }
            Action::EndSession => {
                self.end_session();
                self.card.clear();
                self.card.add_note("Session ended. Send a message to start a new one.", false);
                self.card.set_status("No session", false);
            }
        }
    }

    /// Switches Claude to another project folder. A Claude Code session
    /// belongs to the folder it started in, so this ends the current one; it
    /// stays in Claude Code's history.
    fn set_cwd(&mut self, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        if !path.is_dir() {
            self.card.add_note(&format!("{} isn't a folder.", projects::display(&path)), true);
            return;
        }
        if path == self.cwd {
            return;
        }
        let had_conversation = self.session_id.is_some();
        self.end_session();
        self.card.clear();
        self.cwd = path.clone();
        self.card.set_project(&path);
        self.load_mode();
        let mut note = if projects::is_no_project(&path) {
            "No project: for one-off tasks and questions.".to_string()
        } else {
            format!("Now working in {}.", projects::display(&path))
        };
        if had_conversation {
            note.push_str(" The previous conversation ended; it's still in your Claude Code history.");
        }
        self.card.add_note(&note, false);
        self.card.set_status("No session", false);
        if self.overlay.card_shown()
            && let Err(e) = self.ensure_session() {
                self.card.add_note(&format!("Couldn't reach Claude: {e}"), true);
            }
    }

    /// Loads a past conversation into the card and continues it.
    fn resume(&mut self, id: String) {
        if self.session_id.as_deref() == Some(id.as_str()) {
            return;
        }
        self.end_session();
        self.card.clear();
        let (items, newest) = projects::history(&self.cwd, &id, 80);
        for item in items {
            match item {
                projects::HistoryItem::User(text) => self.card.add_user(&text, &[]),
                projects::HistoryItem::Assistant(text) => self.card.add_text(&text),
                projects::HistoryItem::Tool { name, summary } => self.card.add_tool(&name, &summary),
            }
        }
        self.card.add_note("Resumed. Your next message continues this conversation.", false);
        self.session_id = Some(id);
        self.last_entry = newest;
        self.card.set_session(self.session_id.as_deref());
        if let Err(e) = self.ensure_session() {
            self.card.add_note(&format!("Couldn't reach Claude: {e}"), true);
        }
        self.overlay.show_card(true, true);
    }

    fn choose_cwd(&self) {
        let dialog = gtk4::FileDialog::builder()
            .title("Choose a project folder for Claude")
            .modal(true)
            .initial_folder(&gtk4::gio::File::for_path(&self.cwd))
            .build();
        let tx = self.tx.clone();
        dialog.select_folder(None::<&gtk4::Window>, None::<&gtk4::gio::Cancellable>, move |result| {
            if let Some(path) = result.ok().and_then(|f| f.path()) {
                let _ = tx.send_blocking(Msg::Ui(Action::SetCwd(path)));
            }
        });
    }

    /// Opens this conversation in the Claude desktop app, which imports the
    /// Claude Code session (with its history) into its Code section. Clawd
    /// then lets go of the session so the two don't both write to it.
    fn continue_in_app(&mut self) {
        let Some(id) = self.session_id.clone() else {
            self.card.add_note("Send a message first; there's no conversation to continue yet.", false);
            return;
        };
        if self.running {
            self.card.add_note("Wait for Claude to finish (or press Stop), then continue in the app.", false);
            return;
        }
        let uri = format!("claude://resume?session={id}");
        match gtk4::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>) {
            Ok(()) => {
                self.end_session();
                self.card.add_note(
                    "Continued in the Claude app (Code). This chat has ended here; send a message to start a new one.",
                    false,
                );
                self.card.set_status("Moved to the Claude app", false);
            }
            Err(e) => self.card.add_note(&format!("Couldn't open the Claude app: {e}"), true),
        }
    }

    /// The permission mode saved for the current project ("default" if none).
    fn load_mode(&mut self) {
        self.mode = state::load().modes.get(&self.cwd.to_string_lossy().into_owned()).cloned().unwrap_or_else(|| "default".into());
        self.card.set_mode(&self.mode);
    }

    fn set_mode(&mut self, mode: String) {
        if mode == self.mode {
            return;
        }
        self.mode = mode.clone();
        let project = self.cwd.to_string_lossy().into_owned();
        state::update(|s| {
            s.modes.insert(project.clone(), mode.clone());
        });
        let label = MODES.iter().find(|(m, _, _)| *m == mode).map_or(mode.as_str(), |(_, label, _)| label);
        if let Some(session) = &mut self.session {
            match session.set_permission_mode(&mode) {
                Ok(()) => self.card.add_note(&format!("Mode: {label}"), false),
                Err(e) => self.card.add_note(&format!("Couldn't change the mode: {e}"), true),
            }
        }
    }

    /// Clawd's critical-action check, run before every tool call in every
    /// mode. Anything not critical continues to Claude Code's normal flow.
    fn gate(&mut self, mut req: PermissionRequest) {
        let rules = critical::Rules::load(); // re-read so edits apply right away
        match rules.check(&req.tool_name, &req.input, &self.cwd) {
            None => {
                if let Some(session) = &mut self.session {
                    let _ = session.gate_pass(&req.request_id);
                }
            }
            Some(reason) => {
                req.critical = Some(reason);
                if rules.timeout > 0 {
                    let (tx, id) = (self.tx.clone(), req.request_id.clone());
                    glib::timeout_add_seconds_local_once(rules.timeout as u32, move || {
                        let _ = tx.send_blocking(Msg::CriticalTimeout(id));
                    });
                }
                self.ask(req);
            }
        }
    }

    /// Puts a permission prompt in front of the user. If Clawd is hidden,
    /// a notification says so (with Allow/Deny unless it's critical). A
    /// user-hidden Clawd comes back, since Claude is blocked; one hidden for
    /// a fullscreen app stays out of the way.
    fn ask(&mut self, req: PermissionRequest) {
        if self.overlay.user_hidden() || self.overlay.auto_hidden() {
            self.notify_permission(&req);
        }
        self.pending.push_back(req);
        self.overlay.set_user_hidden(false);
        self.show_current_view();
        self.overlay.show_card(true, false);
    }

    fn notify_permission(&self, req: &PermissionRequest) {
        if !self.settings.notifications {
            return;
        }
        use gtk4::gio::{Notification, NotificationPriority};
        let summary = crate::claude::protocol::tool_summary(&req.tool_name, &req.input);
        let summary: String = summary.lines().next().unwrap_or("").chars().take(120).collect();
        let notification = match &req.critical {
            Some(reason) => {
                let n = Notification::new("Critical action needs your OK");
                n.set_body(Some(&format!("{}: {summary}\n{reason}", req.display_name)));
                n.set_priority(NotificationPriority::Urgent);
                // Critical actions are only approved in Clawd, by holding.
                n.add_button("Open Clawd", "app.show");
                n
            }
            None => {
                let n = Notification::new("Claude needs permission");
                n.set_body(Some(&format!("{}: {summary}", req.display_name)));
                n.set_priority(NotificationPriority::High);
                for (label, decision) in [("Allow", "allow"), ("Deny", "deny")] {
                    let target = format!("{}\n{decision}", req.request_id);
                    n.add_button_with_target_value(label, "app.answer", Some(&target.to_variant()));
                }
                n
            }
        };
        notification.set_default_action("app.show");
        if let Some(app) = self.overlay.application() {
            app.send_notification(Some(&format!("clawd-perm-{}", req.request_id)), &notification);
        }
    }

    fn notify_finished(&self, error: Option<&str>) {
        if !self.settings.notifications {
            return;
        }
        use gtk4::gio::Notification;
        let n = Notification::new(if error.is_some() { "Claude ran into a problem" } else { "Claude finished" });
        let body = error.unwrap_or(&self.reply.last);
        let body: String = body.trim().chars().take(160).collect();
        if !body.is_empty() {
            n.set_body(Some(&body));
        }
        n.set_default_action("app.show");
        if let Some(app) = self.overlay.application() {
            app.send_notification(Some("clawd-done"), &n);
        }
    }

    fn withdraw(&self, id: &str) {
        if let Some(app) = self.overlay.application() {
            app.withdraw_notification(id);
        }
    }

    fn show_tasks(&self) {
        let view: Vec<_> = self.tasks.iter().map(|(_, subject, status, active)| (subject.clone(), status.clone(), active.clone())).collect();
        self.card.set_tasks(&view);
    }

    fn show_usage(&self) {
        self.card.set_context(self.context);
    }

    /// Voice hotkey: start listening, or stop and transcribe.
    fn voice(&mut self) {
        if let Some(recording) = self.recording.take() {
            let wav = recording.stop();
            self.voice_busy = true;
            self.card.set_status("Transcribing…", false);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send_blocking(Msg::Transcribed(crate::voice::transcribe(&wav)));
            });
            return;
        }
        if self.voice_busy {
            return;
        }
        if crate::voice::whisper().is_none() {
            self.voice_note("Voice needs whisper-cpp: sudo pacman -S whisper-cpp");
            return;
        }
        if !crate::voice::model_path().is_some_and(|p| p.exists()) {
            self.voice_busy = true;
            self.voice_note("Downloading the speech model (about 150 MB, once). I'll tell you when it's ready.");
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send_blocking(Msg::ModelDownloaded(crate::voice::download_model()));
            });
            return;
        }
        self.stop_speaking();
        match crate::voice::start_recording() {
            Ok(recording) => {
                let (tx, started) = (self.tx.clone(), recording.started);
                glib::timeout_add_seconds_local_once(crate::voice::MAX_SECONDS as u32, move || {
                    let _ = tx.send_blocking(Msg::VoiceTimeout(started));
                });
                self.recording = Some(recording);
                // Make it obvious that Clawd is listening, and how to finish
                // (without taking the keyboard from whatever you're in).
                let key = &self.settings.hotkeys.voice;
                let finish = if key.is_empty() { "run clawd voice again".to_string() } else { format!("press {key}") };
                self.card.set_status(&format!("🎙 Listening… {finish} to finish"), false);
                self.overlay.set_user_hidden(false);
                self.show_current_view();
                self.overlay.show_card(true, false);
            }
            Err(e) => self.voice_note(&format!("Couldn't record: {e}")),
        }
    }

    /// A note in the card, or a notification if Clawd isn't showing.
    fn voice_note(&self, text: &str) {
        if self.overlay.card_shown() {
            self.card.add_note(text, false);
        } else if let Some(app) = self.overlay.application() {
            let n = gtk4::gio::Notification::new("Clawd voice");
            n.set_body(Some(text));
            app.send_notification(Some("clawd-voice"), &n);
        }
    }

    fn stop_speaking(&mut self) {
        if let Some(mut child) = self.speaking.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The channel settings widgets report on: card actions arrive as
    /// `Msg::Ui` on the app's own channel.
    fn card_actions(&self) -> async_channel::Sender<Action> {
        let (tx, rx) = async_channel::unbounded();
        let app_tx = self.tx.clone();
        glib::spawn_future_local(async move {
            while let Ok(action) = rx.recv().await {
                if app_tx.send(Msg::Ui(action)).await.is_err() {
                    break;
                }
            }
        });
        tx
    }

    fn update_settings(&mut self, settings: crate::state::Settings) {
        let hypr_changed = settings.hotkeys != self.settings.hotkeys || settings.autostart != self.settings.autostart;
        self.settings = settings.clone();
        state::update(|s| s.settings = settings.clone());
        self.overlay.apply_settings(&settings);
        if hypr_changed
            && let Err(e) = crate::hyprconf::apply(&settings) {
                self.card.add_note(&format!("Hyprland reported a config problem: {e}"), true);
            }
    }

    /// Mirrors Clawd's state on the bar button (only when it changes).
    fn refresh_bar(&mut self) {
        let state = if !self.pending.is_empty() {
            BarState::NeedsYou
        } else if self.running {
            BarState::Working
        } else if self.unread {
            BarState::Done
        } else {
            BarState::Idle
        };
        let hidden = self.overlay.user_hidden() || self.overlay.auto_hidden();
        if self.bar != Some((state, hidden)) {
            self.bar = Some((state, hidden));
            bar::show(state, hidden);
        }
    }

    fn set_model(&mut self, model: String) {
        if model == self.model {
            return;
        }
        let name = self.model_name(&model);
        self.model = model.clone();
        state::update(|s| s.model = Some(model.clone()));
        if let Some(session) = &mut self.session {
            match session.set_model(&model) {
                Ok(()) => self.card.add_note(&format!("Switched to {name}"), false),
                Err(e) => self.card.add_note(&format!("Couldn't switch model: {e}"), true),
            }
        }
        self.refresh_efforts();
    }

    fn set_effort(&mut self, level: Option<String>) {
        if level == self.effort {
            return;
        }
        self.effort = level.clone();
        state::update(|s| s.effort = level.clone());
        if let Some(session) = &mut self.session {
            match session.set_effort(level.as_deref()) {
                Ok(()) => self.card.add_note(&format!("Effort: {}", effort_label(level.as_deref())), false),
                Err(e) => self.card.add_note(&format!("Couldn't change effort: {e}"), true),
            }
        }
    }

    /// Shows the current model's effort levels. A level the model doesn't
    /// support falls back to Auto.
    fn refresh_efforts(&mut self) {
        // Until Claude Code's model list arrives there's nothing to check against.
        let Some(model) = self.models.iter().find(|m| m.value == self.model) else { return };
        let levels = model.effort_levels.clone();
        if self.effort.as_ref().is_some_and(|level| !levels.contains(level)) {
            self.effort = None;
            // Saved too, so the next start doesn't pass the old level along.
            state::update(|s| s.effort = None);
            if let Some(session) = &mut self.session {
                let _ = session.set_effort(None);
            }
        }
        self.card.set_efforts(&levels, self.effort.as_deref());
    }

    /// Hides Clawd, lets the user drag out a region, and attaches it. The
    /// capture blocks (slurp waits for the user), so it runs on a thread.
    /// `rect`: capture that rectangle (the focused window); `None`: let the
    /// user select a region.
    fn screenshot(&mut self, rect: Option<(i64, i64, i64, i64)>) {
        if self.capturing {
            return;
        }
        self.capturing = true;
        // Keep Clawd and the card out of the picture.
        self.overlay.set_capturing(true);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Give the compositor a moment to unmap Clawd.
            std::thread::sleep(Duration::from_millis(150));
            let shot = match rect {
                Some(rect) => image::capture_rect(rect),
                None => image::capture_region(),
            };
            let _ = tx.send_blocking(Msg::Captured(shot));
        });
    }

    /// The quick-ask box at the cursor. With `selection`, the message asks
    /// about that text.
    fn quick_ask(&mut self, prefill: &str, selection: Option<String>) {
        let Some(app) = self.overlay.application() else { return };
        let (tx, shown) = (self.tx.clone(), selection.clone());
        crate::quick::open(&app, prefill, shown.as_deref(), move |prompt| {
            let message = match &selection {
                Some(text) => format!("{prompt}\n\n```\n{}\n```", text.trim_end()),
                None => prompt,
            };
            let _ = tx.send_blocking(Msg::Ui(Action::QuickSubmit(message)));
        });
    }

    /// Images are attached; other files are mentioned by path in the draft.
    fn drop_files(&mut self, files: Vec<PathBuf>) {
        let mut mentions = Vec::new();
        for file in files {
            let is_image = file
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&e.to_lowercase().as_str()));
            match std::fs::read(&file) {
                Ok(bytes) if is_image => self.attach(&bytes),
                _ => {
                    let shown = file.strip_prefix(&self.cwd).map(Path::to_path_buf).unwrap_or(file);
                    mentions.push(shown.display().to_string());
                }
            }
        }
        if !mentions.is_empty() {
            self.card.insert_text(&mentions.join(" "));
        }
        self.overlay.set_user_hidden(false);
        self.show_current_view();
        self.overlay.show_card(true, true);
    }

    fn on_captured(&mut self, result: Result<Option<Vec<u8>>, String>) {
        self.capturing = false;
        self.overlay.set_capturing(false);
        match result {
            Ok(Some(png)) => self.attach(&png),
            Ok(None) => {} // cancelled
            Err(e) => {
                self.card.add_note(&format!("Screenshot failed: {e}"), true);
                self.overlay.show_card(true, false);
            }
        }
    }

    fn attach(&mut self, bytes: &[u8]) {
        match image::prepare(bytes) {
            Ok(attachment) => {
                self.attachments.push(attachment);
                self.overlay.set_user_hidden(false);
                self.show_attachments();
                // A pending permission prompt stays on top: Claude is blocked on it.
                self.show_current_view();
                self.overlay.show_card(true, true);
            }
            Err(e) => self.card.add_note(&format!("Couldn't attach the image: {e}"), true),
        }
    }

    fn show_attachments(&self) {
        let thumbs: Vec<_> = self.attachments.iter().map(|a| a.thumb.clone()).collect();
        self.card.set_attachments(&thumbs);
    }

    fn model_name(&self, value: &str) -> String {
        self.models
            .iter()
            .find(|m| m.value == value)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| value.to_string())
    }

    /// The Claude process is gone (it exited, or a write to it failed).
    /// Everything tied to it resets; the session ID is kept so the next
    /// message resumes the conversation.
    fn lose_process(&mut self) {
        self.session = None;
        self.generation += 1; // ignore anything the old process still sends
        self.running = false;
        self.pending.clear();
        self.show_current_view();
        self.card.set_status("", false);
    }

    /// Clawd is quitting: stop Claude and everything it started.
    pub fn shutdown(&mut self) {
        self.recording = None; // stops the microphone
        self.stop_speaking();
        self.end_session();
        bar::stopped();
    }

    /// Stops the Claude process and forgets the conversation.
    fn end_session(&mut self) {
        self.session = None; // dropping it stops Claude and its process group
        self.generation += 1; // ignore anything the old process still sends
        self.session_id = None;
        self.card.set_session(None);
        self.last_entry = None;
        self.last_turn = None;
        self.rewind = None;
        self.running = false;
        self.pending.clear();
        self.tasks.clear();
        self.context = None;
        self.show_tasks();
        self.show_usage();
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Init { session_id } => {
                self.card.set_session(Some(&session_id));
                self.session_id = Some(session_id);
            }
            Event::Models(all) => {
                // "Default (recommended)" is an alias for one of the listed
                // models; show that model selected instead of the alias.
                let default = all.iter().find(|m| m.value == "default").map(|m| m.resolved.clone());
                let models: Vec<ModelInfo> = all.into_iter().filter(|m| m.value != "default").collect();
                if !models.iter().any(|m| m.value == self.model) {
                    let fallback = models.iter().find(|m| Some(&m.resolved) == default.as_ref()).or(models.first());
                    if let Some(m) = fallback {
                        self.model = m.value.clone();
                    }
                }
                self.card.set_models(&models, &self.model);
                self.models = models;
                self.refresh_efforts();
            }
            Event::MessageStart => {
                self.card.begin_assistant();
                self.reply.start_message();
                self.busy = Anim::Think;
            }
            Event::TextDelta(t) => {
                self.reply.add(&t);
                self.card.append_text(&t);
                self.card.set_status("Writing…", true);
                self.busy = Anim::Write;
            }
            Event::Text(t) => {
                self.reply.add(&t);
                self.card.add_text(&t);
            }
            Event::Entry(id) => self.last_entry = Some(id),
            Event::ToolUse { name, summary } => {
                self.card.add_tool(&name, &summary);
                self.card.set_status(&format!("Running {name}…"), true);
                self.busy = Anim::Work(Tool::for_name(&name));
            }
            Event::Permission(req) => self.ask(req),
            Event::Gate(req) => self.gate(req),
            Event::Commands(commands) => self.card.set_commands(commands),
            Event::TaskCreated { id, subject } => {
                self.tasks.retain(|t| t.0 != id);
                self.tasks.push((id, subject, "pending".into(), None));
                self.show_tasks();
            }
            Event::TaskUpdated { id, status, subject, active_form } => {
                if status.as_deref() == Some("deleted") {
                    self.tasks.retain(|t| t.0 != id);
                } else if let Some(task) = self.tasks.iter_mut().find(|t| t.0 == id) {
                    if let Some(s) = status {
                        task.2 = s;
                    }
                    if let Some(s) = subject {
                        task.1 = s;
                    }
                    if active_form.is_some() {
                        task.3 = active_form;
                    }
                }
                self.show_tasks();
            }
            Event::ContextUsage { percent, used, max } => {
                self.context = Some((percent, used, max));
                self.show_usage();
            }
            Event::RateLimitChanged => {
                // A limit changed: refresh the 5-hour/weekly bars.
                if let Some(session) = &mut self.session {
                    let _ = session.request_plan_usage();
                }
            }
            Event::PlanUsage(limits) => self.card.set_plan_usage(&limits),
            Event::PermissionCancelled { request_id } => {
                self.withdraw(&format!("clawd-perm-{request_id}"));
                self.pending.retain(|r| r.request_id != request_id);
                self.show_current_view();
            }
            Event::TurnDone { is_error, error } => {
                self.running = false;
                self.card.finish_reply();
                if let Some(session) = &mut self.session {
                    let _ = session.request_context_usage();
                    let _ = session.request_plan_usage();
                }
                for req in std::mem::take(&mut self.pending) {
                    self.withdraw(&format!("clawd-perm-{}", req.request_id));
                }
                self.show_current_view();
                let long = self.turn_started.take().is_some_and(|t| t.elapsed() >= NOTIFY_AFTER);
                let away = self.overlay.user_hidden() || self.overlay.auto_hidden() || !self.overlay.card_shown();
                if away {
                    self.unread = true;
                    if long {
                        self.notify_finished(error.as_deref().filter(|_| is_error));
                    }
                }
                if let Some(error) = error.filter(|_| is_error) {
                    self.card.add_note(&error, true);
                    self.card.add_reply_actions(&self.reply.turn, self.last_turn.is_some());
                    self.card.set_status("Error", false);
                    self.react(Anim::Error, UPSET_FOR);
                } else {
                    self.card.add_reply_actions(&self.reply.turn, self.last_turn.is_some());
                    self.card.set_status("Done", false);
                    self.react(Anim::Happy, HAPPY_FOR);
                    if self.settings.read_aloud {
                        let text = crate::voice::speakable(&self.reply.last);
                        // Only short replies: long ones are better read than heard.
                        if !text.is_empty() && text.len() <= 600 {
                            self.stop_speaking();
                            self.speaking = crate::voice::speak(&text);
                        }
                    }
                }
            }
            Event::Stderr(line) => eprintln!("claude: {line}"),
            Event::Exited => {
                if self.running {
                    self.card.add_note("Claude exited unexpectedly. Your next message resumes the session.", true);
                    self.react(Anim::Error, UPSET_FOR);
                }
                self.lose_process();
            }
        }
    }

    fn submit(&mut self, text: &str) {
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        // The draft and its screenshots are only cleared once the message has
        // actually gone out, so a failure leaves them ready to retry.
        let images: Vec<_> = self.attachments.iter().map(|a| a.image.clone()).collect();
        match self.send_prompt(text, images) {
            Ok(()) => {
                let attachments = std::mem::take(&mut self.attachments);
                self.show_attachments();
                let previews: Vec<_> = attachments.iter().map(|a| (a.preview.clone(), a.size)).collect();
                self.card.add_user(text, &previews);
            }
            Err(e) => {
                self.card.add_note(&format!("Couldn't reach Claude: {e}. Your message is still in the box."), true);
                self.card.restore_draft(text);
            }
        }
    }

    /// Sends a prompt and starts a turn (the transcript is up to the caller).
    fn send_prompt(&mut self, text: &str, images: Vec<Image>) -> Result<(), String> {
        let uuid = protocol::new_uuid();
        let after = self.last_entry.clone();
        // Sent mid-turn, it joins the turn in progress.
        let joins = self.running;
        let sent = match self.ensure_session() {
            Ok(session) => session.prompt(text, &images, &uuid).map_err(|e| (e.to_string(), true)),
            Err(e) => Err((e, false)),
        };
        if let Err((e, lost)) = sent {
            // A failed write means the process is gone; the next try starts a
            // fresh one that resumes this conversation.
            if lost {
                self.lose_process();
            }
            return Err(e);
        }
        // Claude Code only rewinds a turn that is exactly one prompt and its
        // reply: not one with a message added mid-turn, nor a slash command.
        self.last_turn = (!joins && !text.starts_with('/'))
            .then(|| Turn { uuid, after, text: text.to_string(), images });
        if !joins {
            self.reply.start_turn();
        }
        self.stop_speaking();
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.busy = Anim::Think;
        self.reaction = None;
        self.card.set_status("Thinking…", true);
        Ok(())
    }

    /// Asks the newest prompt again, as if its reply never happened: Claude
    /// restarts with the conversation as it was just before that prompt.
    /// (Changes its tools made to files stay.)
    fn regenerate(&mut self) {
        if self.running || !self.pending.is_empty() {
            return;
        }
        let Some(turn) = self.last_turn.clone() else { return };
        self.session = None;
        self.generation += 1; // ignore anything the old process still sends
        match (&turn.after, &self.session_id) {
            (Some(after), Some(_)) => {
                self.rewind = Some((after.clone(), turn.uuid.clone()));
                self.last_entry = Some(after.clone());
            }
            // It started the conversation: begin a new one (the old one stays
            // in Claude Code's history).
            _ => {
                self.session_id = None;
                self.card.set_session(None);
                self.last_entry = None;
            }
        }
        self.card.remove_last_reply();
        if let Err(e) = self.send_prompt(&turn.text, turn.images) {
            self.card.add_note(&format!("Couldn't regenerate: {e}"), true);
            // Still possible once Claude can be reached.
            self.card.add_reply_actions("", true);
        }
    }

    fn ensure_session(&mut self) -> Result<&mut Session, String> {
        if self.session.is_none() {
            let rewind = self.rewind.as_ref().map(|(keep, drop)| (keep.as_str(), drop.as_str()));
            let (session, rx) = Session::start(&self.cwd, self.session_id.as_deref(), rewind, &self.model, self.effort.as_deref())
                .map_err(|e| format!("couldn't start `claude` ({e})"))?;
            self.rewind = None;
            self.generation += 1;
            let (generation, tx) = (self.generation, self.tx.clone());
            glib::spawn_future_local(async move {
                while let Ok(event) = rx.recv().await {
                    if tx.send(Msg::Claude(generation, event)).await.is_err() {
                        break;
                    }
                }
            });
            self.session = Some(session);
            self.card.set_status("Ready", false);
            if let Some(session) = &mut self.session {
                let _ = session.request_plan_usage();
            }
            if self.mode != "default" {
                let mode = self.mode.clone();
                if let Some(session) = &mut self.session {
                    let _ = session.set_permission_mode(&mode);
                }
            }
        }
        Ok(self.session.as_mut().unwrap())
    }

    fn answer(&mut self, request_id: &str, decision: Decision) {
        self.withdraw(&format!("clawd-perm-{request_id}"));
        // Claude may have withdrawn the request before the click arrived.
        let Some(i) = self.pending.iter().position(|r| r.request_id == request_id) else { return };
        let Some(req) = self.pending.remove(i) else { return };
        if let Some(s) = &mut self.session
            && let Err(e) = s.answer(&req, decision) {
                self.card.add_note(&format!("Couldn't send your answer: {e}"), true);
            }
        let verb = match decision {
            Decision::Allow => "Allowed",
            Decision::AlwaysAllow => "Always allowed",
            Decision::Deny => "Denied",
        };
        self.card.add_note(&format!("{verb} {}", req.display_name), false);
        self.show_current_view();
    }

    /// Permission prompts take over the card until they're answered.
    fn show_current_view(&self) {
        match self.pending.front() {
            Some(req) => {
                let diff = crate::diff::for_tool(&req.tool_name, &req.input, &self.cwd);
                self.card.show_permission(req, 0, self.pending.len(), diff.as_deref());
            }
            // An open settings panel stays open; otherwise back to the chat.
            None if self.card.settings_shown() => {}
            None => self.card.show_chat(),
        }
    }

    /// Shows a short reaction, then goes back to whatever fits.
    fn react(&mut self, anim: Anim, duration: Duration) {
        self.reaction = Some((anim, Instant::now() + duration));
        let tx = self.tx.clone();
        glib::timeout_add_local_once(duration, move || {
            let _ = tx.send_blocking(Msg::Refresh);
        });
    }

    fn refresh_anim(&mut self) {
        if self.reaction.is_some_and(|(_, until)| Instant::now() >= until) {
            self.reaction = None;
        }
        let next = if !self.pending.is_empty() {
            Anim::Ask
        } else if self.recording.is_some() {
            Anim::Listen
        } else if self.voice_busy {
            Anim::Think
        } else if self.running {
            self.busy
        } else if let Some((reaction, _)) = self.reaction {
            reaction
        } else {
            Anim::Idle
        };
        self.anim.set(next);
        // Wander only when nothing needs Clawd to stay put.
        self.overlay.set_wander_allowed(self.pending.is_empty() && !self.running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_text_is_per_turn() {
        let mut reply = ReplyText::default();
        reply.start_message();
        reply.add("Let me look.");
        reply.start_message();
        reply.add("Found ");
        reply.add("it.");
        assert_eq!((reply.turn.as_str(), reply.last.as_str()), ("Let me look.\n\nFound it.", "Found it."));

        // Two turns with replies that weren't streamed (no message start).
        let mut reply = ReplyText::default();
        reply.add("First reply.");
        reply.start_turn();
        reply.add("Second reply.");
        assert_eq!((reply.turn.as_str(), reply.last.as_str()), ("Second reply.", "Second reply."));
    }

    #[test]
    fn periodic_checks_dont_keep_clawd_awake() {
        assert!(!Msg::Environment.is_activity());
        assert!(!Msg::Refresh.is_activity());
        assert!(Msg::Ui(Action::ToggleCard).is_activity());
        assert!(Msg::Claude(0, Event::MessageStart).is_activity());
    }
}
