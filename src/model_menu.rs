//! The footer's model picker: one button showing the model and effort
//! ("✻ Opus 5.5 · Medium"), opening a menu with the model list and an effort
//! slider.

use crate::app::Action;
use crate::card::effort_label;
use crate::claude::protocol::ModelInfo;
use gtk4::prelude::*;
use gtk4::{gdk, glib, DrawingArea, EventControllerKey, GestureDrag, Label, ListBox, MenuButton, Orientation, Popover, Separator};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

const SLIDER_HEIGHT: i32 = 34;
const TROUGH_HEIGHT: f64 = 26.0;
const KNOB_WIDTH: f64 = 28.0;
/// From either end of the slider to the first/last stop.
const STOP_INSET: f64 = KNOB_WIDTH / 2.0 + 4.0;

pub struct ModelMenu {
    inner: Rc<Inner>,
}

struct Inner {
    button: MenuButton,
    name: Label,
    /// "· Medium" on the button; hidden until a level is picked, and for
    /// models without effort levels.
    summary: Label,
    list: ListBox,
    effort_section: gtk4::Box,
    effort_value: Label,
    slider: DrawingArea,
    models: RefCell<Vec<ModelInfo>>,
    /// The current model's effort levels, lowest first.
    levels: RefCell<Vec<String>>,
    /// `None`: none picked, so the model uses its default.
    effort: RefCell<Option<String>>,
    /// The stop under the pointer while dragging (chosen on release).
    dragging: Cell<Option<usize>>,
    actions: async_channel::Sender<Action>,
}

impl ModelMenu {
    pub fn new(actions: async_channel::Sender<Action>) -> ModelMenu {
        let name = Label::new(Some("Loading models…"));
        name.add_css_class("model-name");
        let summary = Label::new(None);
        summary.add_css_class("dim");
        summary.set_visible(false);
        let face = gtk4::Box::new(Orientation::Horizontal, 5);
        face.append(&spark());
        face.append(&name);
        face.append(&summary);
        let button = MenuButton::new();
        button.add_css_class("model");
        button.set_child(Some(&face));
        button.set_always_show_arrow(true);
        button.set_sensitive(false);

        let heading = Label::new(Some("CLAUDE CODE"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        let list = ListBox::new();
        list.add_css_class("models");
        list.set_selection_mode(gtk4::SelectionMode::None);

        let effort_title = Label::new(Some("Effort"));
        effort_title.add_css_class("dim");
        let effort_value = Label::new(None);
        effort_value.add_css_class("effort-value");
        let title_row = gtk4::Box::new(Orientation::Horizontal, 8);
        title_row.append(&effort_title);
        title_row.append(&effort_value);
        let faster = Label::new(Some("Faster"));
        faster.set_xalign(0.0);
        faster.set_hexpand(true);
        let smarter = Label::new(Some("Smarter"));
        smarter.set_xalign(1.0);
        let ends = gtk4::Box::new(Orientation::Horizontal, 0);
        ends.add_css_class("ends");
        ends.append(&faster);
        ends.append(&smarter);
        let slider = DrawingArea::builder().accessible_role(gtk4::AccessibleRole::Slider).build();
        slider.set_content_height(SLIDER_HEIGHT);
        // Arrow keys, Home and End move it; Delete clears it.
        slider.set_focusable(true);
        slider.add_css_class("effort-slider");
        slider.set_hexpand(true);
        slider.set_tooltip_text(Some("How much Claude thinks before answering"));
        let effort_section = gtk4::Box::new(Orientation::Vertical, 6);
        effort_section.append(&Separator::new(Orientation::Horizontal));
        effort_section.append(&title_row);
        effort_section.append(&ends);
        effort_section.append(&slider);
        effort_section.set_visible(false);

        let content = gtk4::Box::new(Orientation::Vertical, 4);
        content.add_css_class("model-menu");
        content.set_size_request(260, -1);
        content.append(&heading);
        content.append(&list);
        content.append(&effort_section);
        let popover = Popover::new();
        popover.set_child(Some(&content));
        button.set_popover(Some(&popover));
        // Above the button when there's room (the arrow still points down).
        popover.set_position(gtk4::PositionType::Top);

        let inner = Rc::new(Inner {
            button,
            name,
            summary,
            list,
            effort_section,
            effort_value,
            slider,
            models: RefCell::default(),
            levels: RefCell::default(),
            effort: RefCell::default(),
            dragging: Cell::new(None),
            actions,
        });

        inner.list.connect_row_activated({
            let inner = Rc::downgrade(&inner);
            move |_, row| {
                if let Some(inner) = inner.upgrade() {
                    inner.pick_model(row.index() as usize);
                }
            }
        });
        inner.slider.set_draw_func({
            let inner = Rc::downgrade(&inner);
            move |_, cr, w, h| {
                if let Some(inner) = inner.upgrade() {
                    inner.draw_slider(cr, w as f64, h as f64);
                }
            }
        });
        // Clicking a stop or dragging to one previews it; releasing picks it.
        // Releasing on the level already picked clears it (the model's default).
        let drag = GestureDrag::new();
        let to_stop = |inner: &Weak<Inner>, x: f64| {
            if let Some(inner) = inner.upgrade() {
                inner.dragging.set(inner.stop_at(x));
                inner.show_effort();
            }
        };
        drag.connect_drag_begin({
            let inner = Rc::downgrade(&inner);
            move |_, x, _| to_stop(&inner, x)
        });
        drag.connect_drag_update({
            let inner = Rc::downgrade(&inner);
            move |gesture, dx, _| {
                if let Some((x, _)) = gesture.start_point() {
                    to_stop(&inner, x + dx);
                }
            }
        });
        drag.connect_drag_end({
            let inner = Rc::downgrade(&inner);
            move |_, _, _| {
                let Some(inner) = inner.upgrade() else { return };
                let level = inner.dragging.take().and_then(|i| inner.levels.borrow().get(i).cloned());
                if let Some(level) = level {
                    let again = inner.effort.borrow().as_ref() == Some(&level);
                    inner.pick_effort((!again).then_some(level));
                }
            }
        });
        inner.slider.add_controller(drag);
        let keys = EventControllerKey::new();
        keys.connect_key_pressed({
            let inner = Rc::downgrade(&inner);
            move |_, key, _, _| {
                let Some(inner) = inner.upgrade() else { return glib::Propagation::Proceed };
                let current = {
                    let (levels, effort) = (inner.levels.borrow(), inner.effort.borrow());
                    levels.iter().position(|l| Some(l) == effort.as_ref())
                };
                if matches!(key, gdk::Key::Delete | gdk::Key::BackSpace) {
                    if current.is_some() {
                        inner.pick_effort(None);
                    }
                    return glib::Propagation::Stop;
                }
                let n = inner.levels.borrow().len();
                let Some(stop) = step(current, n, key) else { return glib::Propagation::Proceed };
                if Some(stop) != current {
                    let level = inner.levels.borrow()[stop].clone();
                    inner.pick_effort(Some(level));
                }
                glib::Propagation::Stop
            }
        });
        inner.slider.add_controller(keys);

        ModelMenu { inner }
    }

    pub fn button(&self) -> &MenuButton {
        &self.inner.button
    }

    /// Fills the menu with Claude Code's models and selects `selected`.
    pub fn set_models(&self, models: &[ModelInfo], selected: &str) {
        let inner = &self.inner;
        while let Some(row) = inner.list.first_child() {
            inner.list.remove(&row);
        }
        for m in models {
            let name = Label::new(Some(&m.display_name));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            let check = gtk4::Image::from_icon_name("object-select-symbolic");
            check.add_css_class("check");
            let row = gtk4::Box::new(Orientation::Horizontal, 10);
            row.set_tooltip_text(Some(&m.description));
            row.append(&spark());
            row.append(&name);
            row.append(&check);
            inner.list.append(&row);
        }
        *inner.models.borrow_mut() = models.to_vec();
        inner.button.set_sensitive(!models.is_empty());
        inner.show_model(models.iter().position(|m| m.value == selected).unwrap_or(0));
    }

    /// Sets the current model's effort levels and selects `selected`
    /// (`None`: none picked). The effort part is hidden for models without
    /// levels.
    pub fn set_efforts(&self, levels: &[String], selected: Option<&str>) {
        let inner = &self.inner;
        *inner.levels.borrow_mut() = levels.to_vec();
        *inner.effort.borrow_mut() = selected.map(str::to_string);
        inner.effort_section.set_visible(!levels.is_empty());
        inner.show_effort();
    }
}

impl Inner {
    fn pick_model(&self, index: usize) {
        let Some(value) = self.models.borrow().get(index).map(|m| m.value.clone()) else { return };
        self.show_model(index);
        let _ = self.actions.send_blocking(Action::SetModel(value));
    }

    fn show_model(&self, index: usize) {
        let models = self.models.borrow();
        let Some(m) = models.get(index) else { return };
        self.name.set_text(&m.display_name);
        self.button.set_tooltip_text(Some(&m.description));
        let mut i = 0;
        while let Some(row) = self.list.row_at_index(i) {
            // Faded rather than hidden, so the rows keep their width.
            if let Some(check) = row.child().and_then(|c| c.last_child()) {
                check.set_opacity(if i as usize == index { 1.0 } else { 0.0 });
            }
            i += 1;
        }
    }

    /// `None` clears the choice, leaving the effort to the model.
    fn pick_effort(&self, level: Option<String>) {
        *self.effort.borrow_mut() = level.clone();
        self.show_effort();
        let _ = self.actions.send_blocking(Action::SetEffort(level));
    }

    fn show_effort(&self) {
        let effort = self.effort.borrow().clone();
        let label = |level: &str| effort_label(Some(level));
        self.summary.set_text(&effort.as_deref().map(|l| format!("· {}", label(l))).unwrap_or_default());
        self.summary.set_visible(effort.is_some() && !self.levels.borrow().is_empty());
        let previewed = self.dragging.get().and_then(|i| self.levels.borrow().get(i).cloned());
        let value = previewed.or(effort).as_deref().map(label).unwrap_or_default();
        self.effort_value.set_text(&value);
        self.slider.update_property(&[gtk4::accessible::Property::ValueText(&value)]);
        self.slider.queue_draw();
    }

    /// The slider stop at `x`, if there are any.
    fn stop_at(&self, x: f64) -> Option<usize> {
        let n = self.levels.borrow().len();
        let span = self.slider.width() as f64 - 2.0 * STOP_INSET;
        match n {
            0 => None,
            1 => Some(0),
            _ => Some(((x - STOP_INSET) / span * (n - 1) as f64).round().clamp(0.0, (n - 1) as f64) as usize),
        }
    }

    fn stop_x(n: usize, i: usize, w: f64) -> f64 {
        if n <= 1 {
            w / 2.0
        } else {
            STOP_INSET + i as f64 * (w - 2.0 * STOP_INSET) / (n - 1) as f64
        }
    }

    /// A rounded trough with a dot per level, and a knob on the chosen one
    /// with the trough shaded up to it (just the dots until one is picked).
    fn draw_slider(&self, cr: &gtk4::cairo::Context, w: f64, h: f64) {
        use std::f64::consts::PI;
        let levels = self.levels.borrow();
        let n = levels.len();
        if n == 0 {
            return;
        }
        let top = (h - TROUGH_HEIGHT) / 2.0;
        rounded_rect(cr, 0.0, top, w, TROUGH_HEIGHT, TROUGH_HEIGHT / 2.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
        let _ = cr.fill();
        let chosen = self.dragging.get().or_else(|| {
            let effort = self.effort.borrow();
            levels.iter().position(|l| Some(l) == effort.as_ref())
        });
        // Shaded up to the knob (and under it, so the rounded end is hidden).
        if let Some(i) = chosen {
            rounded_rect(cr, 0.0, top, Self::stop_x(n, i, w) + KNOB_WIDTH / 2.0, TROUGH_HEIGHT, TROUGH_HEIGHT / 2.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.2);
            let _ = cr.fill();
        }
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
        for i in (0..n).filter(|&i| Some(i) != chosen) {
            cr.arc(Self::stop_x(n, i, w), h / 2.0, 2.5, 0.0, 2.0 * PI);
            let _ = cr.fill();
        }
        if let Some(i) = chosen {
            rounded_rect(cr, Self::stop_x(n, i, w) - KNOB_WIDTH / 2.0, 0.0, KNOB_WIDTH, h, 7.0);
            cr.set_source_rgb(0.93, 0.89, 0.82);
            let _ = cr.fill();
        }
    }
}

/// The slider stop a key moves to from `current` (of `n`), or `None` if the
/// key doesn't move it. With none picked yet, the arrows start at the first.
fn step(current: Option<usize>, n: usize, key: gdk::Key) -> Option<usize> {
    let last = n.checked_sub(1)?;
    match key {
        gdk::Key::Left | gdk::Key::Down => Some(current.map_or(0, |i| i.saturating_sub(1))),
        gdk::Key::Right | gdk::Key::Up => Some(current.map_or(0, |i| (i + 1).min(last))),
        gdk::Key::Home => Some(0),
        gdk::Key::End => Some(last),
        _ => None,
    }
}

/// The Claude spark that marks Claude models.
fn spark() -> Label {
    let spark = Label::new(Some("✻"));
    spark.add_css_class("spark");
    spark
}

fn rounded_rect(cr: &gtk4::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_move_the_effort_slider() {
        assert_eq!(step(Some(2), 5, gdk::Key::Right), Some(3));
        assert_eq!(step(Some(2), 5, gdk::Key::Left), Some(1));
        assert_eq!(step(Some(4), 5, gdk::Key::Right), Some(4), "stops at the end");
        assert_eq!(step(Some(0), 5, gdk::Key::Left), Some(0), "stops at the start");
        assert_eq!(step(Some(2), 5, gdk::Key::Home), Some(0));
        assert_eq!(step(Some(2), 5, gdk::Key::End), Some(4));
        assert_eq!(step(None, 5, gdk::Key::Right), Some(0), "none picked: starts at the first");
        assert_eq!(step(Some(2), 5, gdk::Key::a), None);
        assert_eq!(step(None, 0, gdk::Key::Right), None, "no levels");
    }
}
