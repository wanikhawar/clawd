//! Quick ask: a small prompt box that opens at the cursor (from a hotkey),
//! sends one message to Clawd's session, and gets out of the way.

use crate::hypr;
use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, ApplicationWindow, Entry, EventControllerKey, Label, Orientation};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const WIDTH: i32 = 420;

/// Opens the quick-ask box. `prefill` starts in the entry (selected, so
/// typing replaces it); `context` is shown above it (e.g. the selection
/// being explained). `on_submit` gets the typed text.
pub fn open(app: &Application, prefill: &str, context: Option<&str>, on_submit: impl Fn(String) + 'static) {
    let window = ApplicationWindow::new(app);
    window.set_decorated(false);
    window.set_resizable(false);
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("clawd-quick"));
    // Opened on purpose from a hotkey: take the keyboard right away.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_exclusive_zone(-1);
    place_at_cursor(&window);

    let card = gtk4::Box::new(Orientation::Vertical, 6);
    card.add_css_class("card");
    card.add_css_class("quick");
    card.set_width_request(WIDTH);
    let title = Label::new(None);
    title.set_markup("<b>Ask Claude</b>");
    title.add_css_class("title");
    title.set_xalign(0.0);
    card.append(&title);
    if let Some(context) = context {
        let shown: String = context.trim().chars().take(240).collect();
        let more = if context.trim().chars().count() > 240 { "…" } else { "" };
        let preview = Label::new(Some(&format!("{shown}{more}")));
        preview.add_css_class("code");
        preview.set_xalign(0.0);
        preview.set_wrap(true);
        preview.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        preview.set_lines(4);
        preview.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        card.append(&preview);
    }
    let entry = Entry::new();
    entry.set_text(prefill);
    entry.set_placeholder_text(Some("Ask anything…"));
    card.append(&entry);
    let hint = Label::new(Some("Enter to ask · Esc to cancel · the answer appears with Clawd"));
    hint.add_css_class("dim");
    hint.set_xalign(0.0);
    card.append(&hint);
    window.set_child(Some(&card));

    entry.connect_activate({
        let window = window.downgrade();
        move |entry| {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                on_submit(text);
            }
            if let Some(w) = window.upgrade() {
                w.close();
            }
        }
    });
    let keys = EventControllerKey::new();
    keys.connect_key_pressed({
        let window = window.downgrade();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(w) = window.upgrade() {
                    w.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
    // Clicking elsewhere cancels.
    window.connect_is_active_notify(|w| {
        if !w.is_active() {
            w.close();
        }
    });

    window.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}

/// Puts the window just below and right of the cursor, on the cursor's
/// monitor, kept on screen.
fn place_at_cursor(window: &ApplicationWindow) {
    let Some((cx, cy)) = hypr::cursor_pos() else { return };
    let Some(display) = gdk::Display::default() else { return };
    let monitor = display.monitors().iter::<gdk::Monitor>().filter_map(Result::ok).find(|m| {
        let g = m.geometry();
        (g.x() as f64..(g.x() + g.width()) as f64).contains(&cx) && (g.y() as f64..(g.y() + g.height()) as f64).contains(&cy)
    });
    let Some(monitor) = monitor else { return };
    window.set_monitor(Some(&monitor));
    let g = monitor.geometry();
    let x = (cx as i32 - g.x() + 12).clamp(0, (g.width() - WIDTH - 20).max(0));
    let y = (cy as i32 - g.y() + 12).clamp(0, (g.height() - 220).max(0));
    LayerShell::set_margin(window, Edge::Left, x);
    LayerShell::set_margin(window, Edge::Top, y);
}
