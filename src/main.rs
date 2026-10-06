mod app;
mod bar;
mod card;
mod claude;
mod critical;
mod diff;
mod hypr;
mod hyprconf;
mod image;
mod markdown;
mod projects;
mod quick;
mod settings_page;
mod sprite;
mod state;
mod voice;
mod window;

use app::{App, Msg};
use card::Card;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, Application, CssProvider};
use sprite::Anim;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use claude::protocol::Decision;
use window::Overlay;

const APP_ID: &str = "dev.clawd.Clawd";

/// What a launch asks Clawd to do. If Clawd is already running, the request
/// is forwarded to it (GApplication's single-instance D-Bus activation) and
/// this process exits; otherwise Clawd starts and then does it.
#[derive(Clone, Copy)]
enum Command {
    /// `clawd [PROJECT_DIR]` (app launchers): show Clawd with the card open.
    Show,
    /// `clawd toggle` (bar button): hide or show Clawd.
    Toggle,
    /// `clawd screenshot` (keybind, bar right-click): capture a region.
    Screenshot,
    /// `clawd screenshot-window`: capture the focused window.
    ScreenshotWindow,
    /// `clawd ask`: the quick-ask box at the cursor.
    Ask,
    /// `clawd explain`: quick ask about the selected text.
    Explain,
    /// `clawd voice`: start or stop voice input.
    Voice,
}

impl Command {
    /// The app action that carries this command to the running instance.
    fn action_name(self) -> &'static str {
        match self {
            Command::Show => "show",
            Command::Toggle => "toggle",
            Command::Screenshot => "screenshot",
            Command::ScreenshotWindow => "screenshot-window",
            Command::Ask => "quick-ask",
            Command::Explain => "explain",
            Command::Voice => "voice",
        }
    }
}

fn main() -> glib::ExitCode {
    let arg = std::env::args_os().nth(1);
    let (command, dir) = match arg.as_deref().and_then(|a| a.to_str()) {
        Some("toggle") => (Command::Toggle, None),
        Some("screenshot") => (Command::Screenshot, None),
        Some("screenshot-window") => (Command::ScreenshotWindow, None),
        Some("ask") => (Command::Ask, None),
        Some("explain") => (Command::Explain, None),
        Some("voice") => (Command::Voice, None),
        _ => (Command::Show, arg.map(PathBuf::from)),
    };
    // Resolve relative paths here, against the directory clawd was run from;
    // a running Clawd has its own working directory, so an unresolved path
    // must never be passed along.
    let dir = match dir {
        Some(d) => match d.canonicalize() {
            Ok(path) if path.is_dir() => Some(path),
            _ => {
                eprintln!("clawd: {} isn't a folder", d.display());
                eprintln!("usage: clawd [PROJECT_DIR] | clawd toggle | clawd ask | clawd explain | clawd voice | clawd screenshot | clawd screenshot-window");
                return glib::ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let app = Application::builder().application_id(APP_ID).build();
    // Project folder: `clawd PROJECT_DIR`, else the last one used (launchers
    // start Clawd in ~), else the current directory.
    let cwd = dir
        .clone()
        .or_else(|| state::load().cwd.filter(|p| p.is_dir()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    app.connect_startup(|app| {
        load_css();
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &Application, _, _| app.quit())
            .build();
        app.add_action_entries([quit]);
    });

    // The running app's state, so quitting can stop Claude (and whatever it
    // started) instead of leaving it to outlive Clawd.
    let running: Rc<RefCell<Option<Rc<RefCell<App>>>>> = Rc::default();
    app.connect_shutdown({
        let running = running.clone();
        move |_| {
            if let Some(state) = running.borrow_mut().take() {
                state.borrow_mut().shutdown();
            }
        }
    });

    app.connect_activate(move |app| {
        if app.active_window().is_some() {
            return;
        }
        let (tx, rx) = async_channel::unbounded::<Msg>();
        let tx_env = tx.clone();

        // UI widgets report Actions; wrap them as Msgs for the app loop.
        let (action_tx, action_rx) = async_channel::unbounded();
        glib::spawn_future_local({
            let tx = tx.clone();
            async move {
                while let Ok(action) = action_rx.recv().await {
                    let _ = tx.send(Msg::Ui(action)).await;
                }
            }
        });

        // App actions: Clawd's right-click menu, and what later launches
        // (`clawd`, `clawd toggle`, `clawd screenshot`) forward to us.
        for (name, action) in [
            ("show", app::Action::Show),
            ("toggle", app::Action::Toggle),
            ("screenshot", app::Action::Screenshot),
            ("screenshot-window", app::Action::ScreenshotWindow),
            ("quick-ask", app::Action::QuickAsk),
            ("explain", app::Action::Explain),
            ("settings", app::Action::OpenSettings),
            ("voice", app::Action::Voice),
            ("new-session", app::Action::NewSession),
            ("end-session", app::Action::EndSession),
            ("continue-in-app", app::Action::ContinueInApp),
            ("choose-cwd", app::Action::ChooseCwd),
            ("open-cwd", app::Action::OpenCwd),
        ] {
            let entry = gio::SimpleAction::new(name, None);
            let tx = action_tx.clone();
            entry.connect_activate(move |_, _| {
                let _ = tx.send_blocking(action.clone());
            });
            app.add_action(&entry);
        }

        // Notification buttons: "<request id>\n<allow|deny>".
        let answer = gio::SimpleAction::new("answer", Some(glib::VariantTy::STRING));
        answer.connect_activate({
            let tx = action_tx.clone();
            move |_, target| {
                let Some(target) = target.and_then(|v| v.get::<String>()) else { return };
                let Some((id, decision)) = target.split_once('\n') else { return };
                let decision = if decision == "allow" { Decision::Allow } else { Decision::Deny };
                let _ = tx.send_blocking(app::Action::Answer(id.to_string(), decision));
            }
        });
        app.add_action(&answer);

        // Project folder menu entries carry the folder as a string target.
        let set_cwd = gio::SimpleAction::new("set-cwd", Some(glib::VariantTy::STRING));
        set_cwd.connect_activate({
            let tx = action_tx.clone();
            move |_, target| {
                if let Some(path) = target.and_then(|v| v.get::<String>()) {
                    let _ = tx.send_blocking(app::Action::SetCwd(PathBuf::from(path)));
                }
            }
        });
        app.add_action(&set_cwd);

        let resume = gio::SimpleAction::new("resume", Some(glib::VariantTy::STRING));
        resume.connect_activate({
            let tx = action_tx.clone();
            move |_, target| {
                if let Some(id) = target.and_then(|v| v.get::<String>()) {
                    let _ = tx.send_blocking(app::Action::Resume(id));
                }
            }
        });
        app.add_action(&resume);

        let anim = Rc::new(Cell::new(Anim::Idle));
        let activity = Rc::new(Cell::new(Instant::now()));
        let card = Rc::new(Card::new(action_tx.clone()));
        let overlay = Overlay::build(app, card.clone(), anim.clone(), activity.clone(), action_tx);
        let state = Rc::new(RefCell::new(App::new(overlay, card, anim, activity, tx, cwd.clone())));
        running.replace(Some(state.clone()));

        // Step aside for fullscreen apps and gamer mode: re-check whenever
        // Hyprland reports a window/workspace change, and every 2s for gamer
        // mode (a file appearing, which Hyprland doesn't report).
        let watcher = tx_env.clone();
        crate::hypr::watch(move || {
            let _ = watcher.send_blocking(Msg::Environment);
        });
        glib::timeout_add_seconds_local(2, move || {
            let _ = tx_env.send_blocking(Msg::Environment);
            glib::ControlFlow::Continue
        });

        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                state.borrow_mut().handle(msg);
            }
        });

        // Do what this launch was for. A fresh start is already "shown", so
        // a toggle has nothing left to do (it would hide Clawd straight away).
        if !matches!(command, Command::Toggle) {
            app.activate_action(command.action_name(), None);
        }
    });

    // Registering emits "startup" in the primary instance, so the handlers
    // above must be connected first (otherwise e.g. Quit never exists).
    if let Err(e) = app.register(gio::Cancellable::NONE) {
        eprintln!("clawd: {e}");
        return glib::ExitCode::FAILURE;
    }
    if app.is_remote() {
        // `clawd PROJECT_DIR` while Clawd runs: switch it to that folder first.
        if let Some(dir) = &dir {
            app.activate_action("set-cwd", Some(&dir.to_string_lossy().to_variant()));
        }
        app.activate_action(command.action_name(), None);
        if let Some(bus) = app.dbus_connection() {
            let _ = bus.flush_sync(gio::Cancellable::NONE);
        }
        // Exit without finalizing the GApplication: it never ran, and GLib
        // would warn that it didn't unregister from D-Bus.
        std::process::exit(0);
    }

    // GTK would otherwise try to interpret our arguments.
    app.run_with_args(&[] as &[&str])
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
