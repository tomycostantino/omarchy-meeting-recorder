//! The teleprompter: a panel next to the recorder with the script of a
//! voice-over, to read from while recording and to edit on the spot.
//!
//! `omarchy-meeting-recorder teleprompter <file>` opens it in the recorder
//! window, and so does the [a] in the corner of the recording page. The paragraph with the
//! cursor is lit and the others are dimmed. Ctrl+↓ and Ctrl+↑, or the buttons
//! above the text, move to the next and the previous paragraph and keep it in
//! the middle. Every edit is written back to the file, so whoever made the
//! script reads what you meant to say.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::APP_NAME;

/// The script the [a] button opens when none was open yet: a file of the
/// app's own, made empty the first time, to type a script in.
pub fn default_script() -> PathBuf {
    let path = glib::user_data_dir().join(APP_NAME).join("teleprompter.md");
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, "");
    }
    path
}

/// The paragraphs of `text` as (start, end) in chars: runs of lines that are
/// not blank, from the first char of the first line to the end of the last.
pub fn paragraphs(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut offset = 0;
    for line in text.split('\n') {
        let length = line.chars().count();
        if line.trim().is_empty() {
            found.extend(open.take());
        } else {
            let start = open.map_or(offset, |(start, _)| start);
            open = Some((start, offset + length));
        }
        offset += length + 1;
    }
    found.extend(open);
    found
}

/// The paragraph at char `offset`: the one it is in, else the next one, else the last.
pub fn paragraph_at(paragraphs: &[(usize, usize)], offset: usize) -> Option<usize> {
    paragraphs
        .iter()
        .position(|&(_, end)| offset <= end)
        .or_else(|| paragraphs.len().checked_sub(1))
}

pub struct Teleprompter {
    pub widget: gtk::Box,
    pub close_button: gtk::Button,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    file_label: gtk::Label,
    position_label: gtk::Label,
    dim: gtk::TextTag,
    path: RefCell<Option<PathBuf>>,
    /// The script that was open last, for the [a] button to bring back.
    last: RefCell<Option<PathBuf>>,
    /// Set while the file is put in the buffer, so that is not saved again.
    loading: Cell<bool>,
    /// Bumped on every edit; a save only runs when no edit came after it.
    edits: Cell<u64>,
    /// The paragraph that was lit last, to scroll only when the cursor leaves it.
    lit: Cell<Option<usize>>,
    /// The scroll under way, stopped when the next one starts.
    scrolling: RefCell<Option<adw::TimedAnimation>>,
    /// A mouse button is down in the text. Scrolling then would move the text
    /// under the pointer and turn the click into a selection, so it waits.
    pressing: Cell<bool>,
    /// A scroll that waits for the button to come up.
    scroll_waiting: Cell<bool>,
}

impl Teleprompter {
    /// The panel looks like Omawrite: a centred column of monospaced text on
    /// the theme's background, and a quiet footer with the file and the controls.
    pub fn new() -> Rc<Self> {
        let view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            // Room above the first paragraph and below the last, so both can sit in the middle.
            .top_margin(220)
            .bottom_margin(320)
            .pixels_below_lines(4)
            .css_classes(["teleprompter-text"])
            .build();
        let dim = gtk::TextTag::new(Some("dim"));
        view.buffer().tag_table().add(&dim);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        // A column of about 65 characters in the middle, however wide the panel is.
        // After the layout pass: changing the text view while GTK lays it out aborts.
        let weak = view.downgrade();
        scroller.hadjustment().connect_changed(move |adj| {
            let side = ((adj.page_size() - 680.0) / 2.0).max(32.0) as i32;
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                let Some(view) = weak.upgrade() else { return };
                if view.left_margin() != side {
                    view.set_left_margin(side);
                    view.set_right_margin(side);
                }
            });
        });

        let file_label = gtk::Label::builder()
            .css_classes(["teleprompter-footer"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let position_label = gtk::Label::builder()
            .css_classes(["teleprompter-footer"])
            .margin_end(6)
            .build();
        let button = |icon: &str, tooltip: &str| {
            gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .css_classes(["flat", "teleprompter-button"])
                .valign(gtk::Align::Center)
                .build()
        };
        let previous_button = button("go-up-symbolic", "Previous paragraph (Ctrl+↑)");
        let next_button = button("go-down-symbolic", "Next paragraph (Ctrl+↓)");
        let close_button = button("window-close-symbolic", "Close the teleprompter");
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        footer.set_margin_top(4);
        footer.set_margin_bottom(6);
        footer.set_margin_start(18);
        footer.set_margin_end(8);
        footer.append(&file_label);
        footer.append(&position_label);
        footer.append(&previous_button);
        footer.append(&next_button);
        footer.append(&close_button);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("teleprompter");
        widget.set_hexpand(true);
        widget.set_visible(false);
        widget.append(&scroller);
        widget.append(&footer);

        let prompter = Rc::new(Teleprompter {
            widget,
            close_button,
            view,
            scroller,
            file_label,
            position_label,
            dim,
            path: RefCell::default(),
            last: RefCell::default(),
            loading: Cell::new(false),
            edits: Cell::new(0),
            lit: Cell::new(None),
            scrolling: RefCell::default(),
            pressing: Cell::new(false),
            scroll_waiting: Cell::new(false),
        });

        let buttons = gtk::EventControllerLegacy::new();
        let weak = Rc::downgrade(&prompter);
        buttons.connect_event(move |_, event| {
            let Some(prompter) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match event.event_type() {
                gdk::EventType::ButtonPress => prompter.pressing.set(true),
                gdk::EventType::ButtonRelease => {
                    prompter.pressing.set(false);
                    if prompter.scroll_waiting.replace(false) {
                        prompter.recenter_animated();
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        prompter.view.add_controller(buttons);

        let buffer = prompter.view.buffer();
        let weak = Rc::downgrade(&prompter);
        buffer.connect_cursor_position_notify(move |_| {
            if let Some(prompter) = weak.upgrade() {
                prompter.highlight();
            }
        });
        let weak = Rc::downgrade(&prompter);
        buffer.connect_changed(move |_| {
            if let Some(prompter) = weak.upgrade() {
                prompter.highlight();
                prompter.save_soon();
            }
        });

        let weak = Rc::downgrade(&prompter);
        prompter.scroller.vadjustment().connect_changed(move |_| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(prompter) = weak.upgrade() {
                    prompter.recenter();
                }
            });
        });

        for (button, forward) in [(&previous_button, false), (&next_button, true)] {
            let weak = Rc::downgrade(&prompter);
            button.connect_clicked(move |_| {
                if let Some(prompter) = weak.upgrade() {
                    prompter.jump(forward);
                }
            });
        }
        // Global: they work from anywhere in the window, also while the text has no focus.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Global);
        for (key, forward) in [(gdk::Key::Down, true), (gdk::Key::Up, false)] {
            let weak = Rc::downgrade(&prompter);
            let action = gtk::CallbackAction::new(move |_, _| {
                match weak.upgrade() {
                    Some(prompter) if prompter.widget.is_visible() => prompter.jump(forward),
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            });
            let trigger = gtk::KeyvalTrigger::new(key, gdk::ModifierType::CONTROL_MASK);
            shortcuts.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
        }
        prompter.widget.add_controller(shortcuts);
        prompter
    }

    pub fn is_loaded(&self) -> bool {
        self.path.borrow().is_some()
    }

    /// Puts the script in `path` in the panel. Returns false when it cannot be read.
    pub fn load(&self, path: &Path) -> bool {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("{APP_NAME}: could not read {}: {err}", path.display());
                return false;
            }
        };
        let buffer = self.view.buffer();
        self.loading.set(true);
        buffer.set_text(&text);
        self.loading.set(false);
        buffer.place_cursor(&buffer.start_iter());
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        self.file_label.set_label(&name);
        self.file_label.set_tooltip_text(Some(&path.display().to_string()));
        *self.path.borrow_mut() = Some(path.to_owned());
        self.lit.set(None);
        self.view.grab_focus();
        self.highlight();
        true
    }

    pub fn unload(&self) {
        *self.last.borrow_mut() = self.path.take();
    }

    /// The script that was open before the teleprompter was closed.
    pub fn last_path(&self) -> Option<PathBuf> {
        self.last.borrow().clone()
    }

    /// Lights the paragraph with the cursor and dims the rest.
    fn highlight(&self) {
        let buffer = self.view.buffer();
        let (start, end) = buffer.bounds();
        buffer.remove_tag(&self.dim, &start, &end);
        let color = self.view.color();
        self.dim.set_foreground_rgba(Some(&gdk::RGBA::new(
            color.red(),
            color.green(),
            color.blue(),
            0.28,
        )));
        let text = buffer.text(&start, &end, false);
        let paragraphs = paragraphs(&text);
        let cursor = usize::try_from(buffer.cursor_position()).unwrap_or(0);
        self.position_label.set_label("");
        if let Some(index) = paragraph_at(&paragraphs, cursor) {
            self.position_label
                .set_label(&format!("{} / {}", index + 1, paragraphs.len()));
            let (from, to) = paragraphs[index];
            buffer.apply_tag(&self.dim, &start, &buffer.iter_at_offset(from as i32));
            buffer.apply_tag(&self.dim, &buffer.iter_at_offset(to as i32), &end);
            if self.lit.replace(Some(index)) != Some(index) {
                if self.pressing.get() {
                    self.scroll_waiting.set(true);
                } else {
                    self.scroll_to(from, to, true);
                }
            }
        }
    }

    /// Scrolls the paragraph from char `from` to `to` to the middle of the panel.
    fn scroll_to(&self, from: usize, to: usize, animate: bool) {
        let buffer = self.view.buffer();
        let top = self.view.iter_location(&buffer.iter_at_offset(from as i32));
        let bottom = self.view.iter_location(&buffer.iter_at_offset(to as i32));
        // Where the paragraph is now, measured from the top of what is visible.
        let (_, top) = self
            .view
            .buffer_to_window_coords(gtk::TextWindowType::Widget, 0, top.y());
        let (_, bottom) = self.view.buffer_to_window_coords(
            gtk::TextWindowType::Widget,
            0,
            bottom.y() + bottom.height(),
        );
        let adj = self.scroller.vadjustment();
        let highest = (adj.upper() - adj.page_size()).max(adj.lower());
        let middle = f64::from(top + bottom) / 2.0;
        let to = (adj.value() + middle - adj.page_size() / 2.0).clamp(adj.lower(), highest);
        if let Some(running) = self.scrolling.take() {
            running.pause();
        }
        if !animate {
            adj.set_value(to);
            return;
        }
        let from = adj.value();
        let target = adw::CallbackAnimationTarget::new(move |value| adj.set_value(value));
        let animation = adw::TimedAnimation::new(&self.scroller, from, to, 240, target);
        animation.play();
        *self.scrolling.borrow_mut() = Some(animation);
    }

    /// Scrolls the lit paragraph to the middle, the way moving to it does.
    fn recenter_animated(&self) {
        let buffer = self.view.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let paragraphs = paragraphs(&text);
        if let Some((from, to)) = self.lit.get().and_then(|index| paragraphs.get(index).copied()) {
            self.scroll_to(from, to, true);
        }
    }

    /// Puts the lit paragraph back in the middle, without animating: after a
    /// new layout, a resize, or when the panel comes on screen.
    fn recenter(&self) {
        if self.pressing.get() {
            return;
        }
        let buffer = self.view.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let paragraphs = paragraphs(&text);
        if let Some((from, to)) = self.lit.get().and_then(|index| paragraphs.get(index).copied()) {
            self.scroll_to(from, to, false);
        }
    }

    /// Moves the cursor to the start of the next or the previous paragraph;
    /// lighting it scrolls it to the middle.
    fn jump(&self, forward: bool) {
        let buffer = self.view.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let paragraphs = paragraphs(&text);
        let cursor = usize::try_from(buffer.cursor_position()).unwrap_or(0);
        let Some(index) = paragraph_at(&paragraphs, cursor) else {
            return;
        };
        let target = if forward {
            (index + 1).min(paragraphs.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        buffer.place_cursor(&buffer.iter_at_offset(paragraphs[target].0 as i32));
    }

    /// Writes the text back to the file half a second after the last edit.
    fn save_soon(self: &Rc<Self>) {
        if self.loading.get() {
            return;
        }
        let edit = self.edits.get() + 1;
        self.edits.set(edit);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            let Some(prompter) = weak.upgrade() else {
                return;
            };
            if prompter.edits.get() != edit {
                return;
            }
            let Some(path) = prompter.path.borrow().clone() else {
                return;
            };
            let buffer = prompter.view.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            if let Err(err) = std::fs::write(&path, text.as_str()) {
                eprintln!("{APP_NAME}: could not save {}: {err}", path.display());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_are_runs_of_lines_between_blank_ones() {
        let script = "Het veld is weg.\nEven uitleggen waarom.\n\n  \nVroeger wél.\n";
        let found = paragraphs(script);
        assert_eq!(found, vec![(0, 39), (44, 56)]);
        let chars: Vec<char> = script.chars().collect();
        let text = |(start, end): (usize, usize)| chars[start..end].iter().collect::<String>();
        assert_eq!(text(found[0]), "Het veld is weg.\nEven uitleggen waarom.");
        assert_eq!(text(found[1]), "Vroeger wél.");
        assert!(paragraphs("\n \n").is_empty());
    }

    #[test]
    fn the_cursor_in_a_blank_line_belongs_to_the_next_paragraph() {
        let found = paragraphs("Een.\n\nTwee.\n\nDrie.");
        assert_eq!(paragraph_at(&found, 0), Some(0));
        assert_eq!(paragraph_at(&found, 4), Some(0));
        assert_eq!(paragraph_at(&found, 5), Some(1));
        assert_eq!(paragraph_at(&found, 99), Some(2));
        assert_eq!(paragraph_at(&[], 3), None);
    }
}
