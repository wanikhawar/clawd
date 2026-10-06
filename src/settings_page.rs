//! The settings panel shown in Clawd's card.
//!
//! Built fresh each time it opens from the current settings; every change
//! is sent to the app as a whole new `Settings` value.

use crate::app::Action;
use crate::hyprconf;
use crate::state::Settings;
use gtk4::prelude::*;
use gtk4::{Align, Button, DropDown, Entry, Grid, Label, Orientation, PolicyType, ScrolledWindow, Switch};
use std::cell::RefCell;
use std::rc::Rc;

/// Updates one on/off setting.
type SetFlag = fn(&mut Settings, bool);

const SIZES: [(f64, &str); 3] = [(1.0, "Small"), (1.5, "Medium"), (2.0, "Large")];
const SLEEP: [(u64, &str); 5] = [(30, "After 30 s"), (90, "After 90 s"), (180, "After 3 min"), (600, "After 10 min"), (0, "Never")];

pub fn build(settings: &Settings, actions: async_channel::Sender<Action>) -> gtk4::Widget {
    let current = Rc::new(RefCell::new(settings.clone()));
    let send = {
        let (current, actions) = (current.clone(), actions.clone());
        move || {
            let _ = actions.send_blocking(Action::UpdateSettings(current.borrow().clone()));
        }
    };

    let page = gtk4::Box::new(Orientation::Vertical, 10);
    let top = gtk4::Box::new(Orientation::Horizontal, 6);
    let title = Label::new(Some("Settings"));
    title.add_css_class("title");
    title.set_hexpand(true);
    title.set_xalign(0.0);
    let done = Button::with_label("Done");
    done.add_css_class("allow");
    done.connect_clicked({
        let actions = actions.clone();
        move |_| {
            let _ = actions.send_blocking(Action::CloseSettings);
        }
    });
    top.append(&title);
    top.append(&done);
    page.append(&top);

    let body = gtk4::Box::new(Orientation::Vertical, 10);
    let scroller = ScrolledWindow::new();
    scroller.set_hscrollbar_policy(PolicyType::Never);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(380);
    scroller.set_child(Some(&body));
    page.append(&scroller);

    // ---- Clawd ----
    body.append(&heading("Clawd"));
    let rows = Grid::new();
    rows.set_row_spacing(6);
    rows.set_column_spacing(10);
    let mut row = 0;

    let size = DropDown::from_strings(&SIZES.map(|(_, n)| n));
    size.set_selected(SIZES.iter().position(|(s, _)| (*s - settings.scale).abs() < 0.01).unwrap_or(0) as u32);
    size.connect_selected_notify({
        let (current, send) = (current.clone(), send.clone());
        move |d| {
            current.borrow_mut().scale = SIZES[d.selected() as usize].0;
            send();
        }
    });
    add_row(&rows, &mut row, "Size", &size);

    let sleep = DropDown::from_strings(&SLEEP.map(|(_, n)| n));
    sleep.set_selected(SLEEP.iter().position(|(s, _)| *s == settings.sleep_after).unwrap_or(1) as u32);
    sleep.connect_selected_notify({
        let (current, send) = (current.clone(), send.clone());
        move |d| {
            current.borrow_mut().sleep_after = SLEEP[d.selected() as usize].0;
            send();
        }
    });
    add_row(&rows, &mut row, "Doze off", &sleep);

    let toggles: [(&str, bool, SetFlag); 4] = [
        ("Wander when idle", settings.wander, |s, v| s.wander = v),
        ("Notify while hidden", settings.notifications, |s, v| s.notifications = v),
        ("Start with Hyprland", settings.autostart, |s, v| s.autostart = v),
        ("Read short replies aloud", settings.read_aloud, |s, v| s.read_aloud = v),
    ];
    for (label, value, set) in toggles {
        let switch = Switch::new();
        switch.set_active(value);
        switch.set_halign(Align::Start);
        switch.connect_active_notify({
            let (current, send) = (current.clone(), send.clone());
            move |sw| {
                set(&mut current.borrow_mut(), sw.is_active());
                send();
            }
        });
        add_row(&rows, &mut row, label, &switch);
    }
    body.append(&rows);

    // ---- Safety ----
    body.append(&heading("Safety"));
    let rules = Button::with_label("Edit critical-action rules…");
    rules.set_halign(Align::Start);
    rules.set_tooltip_text(Some("~/.config/clawd/critical.toml: what always asks, even in Bypass mode"));
    rules.connect_clicked({
        let actions = actions.clone();
        move |_| {
            let _ = actions.send_blocking(Action::EditCriticalRules);
        }
    });
    body.append(&rules);

    // ---- Hotkeys ----
    body.append(&heading("Hotkeys"));
    let keys = Grid::new();
    keys.set_row_spacing(6);
    keys.set_column_spacing(10);
    let hk = &settings.hotkeys;
    let fields: Vec<(&str, Entry)> = [
        ("Quick ask", &hk.quick_ask),
        ("Explain selection", &hk.explain),
        ("Screenshot region", &hk.screenshot),
        ("Screenshot window", &hk.screenshot_window),
        ("Show / hide", &hk.toggle),
        ("Voice", &hk.voice),
    ]
    .into_iter()
    .map(|(label, value)| {
        let entry = Entry::new();
        entry.set_text(value);
        entry.set_placeholder_text(Some("e.g. SUPER + A (empty: none)"));
        entry.set_hexpand(true);
        (label, entry)
    })
    .collect();
    let mut row = 0;
    for (label, entry) in &fields {
        add_row(&keys, &mut row, label, entry);
    }
    body.append(&keys);

    let status = Label::new(None);
    status.add_css_class("dim");
    status.set_xalign(0.0);
    status.set_wrap(true);
    if !hyprconf::is_loaded() {
        status.set_text("Hotkeys need this line in ~/.config/hypr/hyprland.lua:\npcall(require, \"clawd\")");
    }
    let save = Button::with_label("Save hotkeys");
    save.set_halign(Align::Start);
    save.connect_clicked({
        let (current, send, status) = (current.clone(), send.clone(), status.clone());
        move |_| {
            let mut bad = Vec::new();
            let mut values = Vec::new();
            for (label, entry) in &fields {
                let text = entry.text().trim().to_string();
                entry.remove_css_class("error");
                if text.is_empty() {
                    values.push(String::new());
                } else if let Some(combo) = hyprconf::parse_combo(&text) {
                    entry.set_text(&combo);
                    values.push(combo);
                } else {
                    entry.add_css_class("error");
                    bad.push(*label);
                }
            }
            if !bad.is_empty() {
                status.set_text(&format!("Not a key combo: {}. Use e.g. SUPER + A.", bad.join(", ")));
                return;
            }
            {
                let hk = &mut current.borrow_mut().hotkeys;
                let mut v = values.into_iter();
                for slot in [&mut hk.quick_ask, &mut hk.explain, &mut hk.screenshot, &mut hk.screenshot_window, &mut hk.toggle, &mut hk.voice] {
                    *slot = v.next().unwrap_or_default();
                }
            }
            status.set_text("Saved.");
            send();
        }
    });
    body.append(&save);
    body.append(&status);

    page.upcast()
}

fn heading(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class("section");
    label.set_xalign(0.0);
    label
}

fn add_row(grid: &Grid, row: &mut i32, label: &str, widget: &impl IsA<gtk4::Widget>) {
    let name = Label::new(Some(label));
    name.set_xalign(0.0);
    name.add_css_class("dim");
    grid.attach(&name, 0, *row, 1, 1);
    grid.attach(widget, 1, *row, 1, 1);
    *row += 1;
}
