//! Minimal Hyprland IPC client.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn socket_dir() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(PathBuf::from(runtime).join("hypr").join(instance))
}

fn request(cmd: &str) -> Option<String> {
    let mut stream = UnixStream::connect(socket_dir()?.join(".socket.sock")).ok()?;
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

fn json(cmd: &str) -> Option<Value> {
    serde_json::from_str(&request(&format!("j/{cmd}"))?).ok()
}

/// The cursor's position in global layout coordinates (logical pixels),
/// or None when not running under Hyprland.
pub fn cursor_pos() -> Option<(f64, f64)> {
    let reply = json("cursorpos")?;
    Some((reply["x"].as_f64()?, reply["y"].as_f64()?))
}

/// Whether a window is truly fullscreen (not just maximized) on the active
/// workspace of `monitor` (a connector name like "DP-2").
pub fn fullscreen_on(monitor: &str) -> bool {
    let Some(monitors) = json("monitors") else { return false };
    let Some(workspace) = monitors
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"] == monitor)
        .and_then(|m| m["activeWorkspace"]["id"].as_i64())
    else {
        return false;
    };
    let Some(clients) = json("clients") else { return false };
    // Hyprland's fullscreen modes: 0 none, 1 maximized, 2 fullscreen.
    clients
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["workspace"]["id"].as_i64() == Some(workspace) && c["fullscreen"].as_i64().unwrap_or(0) >= 2)
}

/// The focused window's geometry (x, y, width, height) in layout coordinates.
pub fn active_window_geometry() -> Option<(i64, i64, i64, i64)> {
    let w = json("activewindow")?;
    let at = w["at"].as_array()?;
    let size = w["size"].as_array()?;
    Some((at.first()?.as_i64()?, at.get(1)?.as_i64()?, size.first()?.as_i64()?, size.get(1)?.as_i64()?))
}

/// Calls `on_change` (on a background thread) whenever windows or workspaces
/// change in a way that could affect fullscreen state or window positions.
/// Returns false when not running under Hyprland.
pub fn watch(on_change: impl Fn() + Send + 'static) -> bool {
    let Some(stream) = socket_dir().and_then(|d| UnixStream::connect(d.join(".socket2.sock")).ok()) else {
        return false;
    };
    std::thread::spawn(move || {
        const RELEVANT: [&str; 9] = [
            "fullscreen>>",
            "workspace>>",
            "workspacev2>>",
            "focusedmon>>",
            "activewindow>>",
            "openwindow>>",
            "closewindow>>",
            "movewindow>>",
            "changefloatingmode>>",
        ];
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if RELEVANT.iter().any(|e| line.starts_with(e)) {
                on_change();
            }
        }
    });
    true
}
