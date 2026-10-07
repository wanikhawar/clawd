//! Persistent app state (~/.config/clawd/state.json).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct State {
    /// Clawd's top-left corner, relative to its monitor's top-left.
    pub x: i32,
    pub y: i32,
    /// Connector name of the monitor Clawd was on (e.g. "DP-2").
    #[serde(default)]
    pub monitor: Option<String>,
    /// Model for new sessions, as Claude Code names it ("default", "sonnet", …).
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort for new sessions; `None` = the model's default.
    #[serde(default)]
    pub effort: Option<String>,
    /// Permission mode chosen for each project folder.
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub settings: Settings,
}

/// What the settings panel controls.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Clawd's size: 1.0, 1.5 or 2.0.
    pub scale: f64,
    /// Seconds idle before Clawd dozes off; 0 = never.
    pub sleep_after: u64,
    /// Walk around / perch on the focused window when idle.
    pub wander: bool,
    /// Notifications while Clawd is hidden.
    pub notifications: bool,
    /// Start Clawd with Hyprland.
    pub autostart: bool,
    /// Read short replies aloud (needs espeak-ng).
    pub read_aloud: bool,
    /// Hotkeys, as Hyprland key combos ("SUPER + A"); empty = no hotkey.
    pub hotkeys: Hotkeys,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Hotkeys {
    pub quick_ask: String,
    pub explain: String,
    pub screenshot: String,
    pub screenshot_window: String,
    pub toggle: String,
    pub voice: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            scale: 1.0,
            sleep_after: 90,
            wander: true,
            notifications: true,
            autostart: false,
            read_aloud: false,
            hotkeys: Hotkeys::default(),
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            quick_ask: "SUPER + A".into(),
            explain: "SUPER + CTRL + A".into(),
            screenshot: "SUPER + ALT + A".into(),
            screenshot_window: "SUPER + ALT + SHIFT + A".into(),
            toggle: "SUPER + CTRL + SHIFT + A".into(),
            voice: "SUPER + Z".into(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self { x: 80, y: 80, monitor: None, model: None, effort: None, modes: Default::default(), settings: Settings::default() }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("clawd").join("state.json"))
}

pub fn load() -> State {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Loads, changes and saves the state, so callers only touch their own fields.
pub fn update(change: impl FnOnce(&mut State)) {
    let mut state = load();
    change(&mut state);
    save(&state);
}

fn save(state: &State) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(state)
        && let Err(e) = std::fs::write(&p, json) {
            eprintln!("clawd: couldn't save state to {}: {e}", p.display());
        }
}
