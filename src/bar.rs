//! Clawd's state on the Noctalia bar button.
//!
//! The button itself is defined in the user's Noctalia config (so it works
//! while Clawd isn't running). Noctalia merges every `*.toml` in its config
//! folder and reloads on change, so Clawd overlays the live glyph, label and
//! tooltip from a file of its own, and removes that file when it quits.

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarState {
    Idle,
    Working,
    /// A permission prompt is waiting.
    NeedsYou,
    /// Claude finished while Clawd wasn't showing.
    Done,
}

fn path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("noctalia");
    // Only when Noctalia is set up; never create its config folder.
    dir.is_dir().then(|| dir.join("50-clawd.toml"))
}

/// Writes the button's look for `state`.
pub fn show(state: BarState, hidden: bool) {
    let Some(path) = path() else { return };
    let (glyph, label, what) = match state {
        BarState::Idle => ("message-chatbot", "", "ready"),
        BarState::Working => ("message-2-cog", "", "Claude is working"),
        BarState::NeedsYou => ("message-2-exclamation", "!", "Claude needs your answer"),
        BarState::Done => ("message-2-check", "", "Claude finished"),
    };
    let visibility = if hidden { " (hidden)" } else { "" };
    let text = format!(
        "# Written by Clawd while it runs: its live state on the bar button.\n\
         # The button itself is defined in another file here; this one is\n\
         # removed when Clawd quits.\n\
         [widget.clawd]\n\
         glyph = \"{glyph}\"\n\
         label = \"{label}\"\n\
         tooltip = \"Clawd{visibility} · {what} · click: show/hide · right-click: screenshot for Claude\"\n"
    );
    // Write-then-rename, so Noctalia never reads a half-written file.
    let tmp = path.with_extension("toml.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Clawd is quitting: back to the button's own (static) look.
pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}
