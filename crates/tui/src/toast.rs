//! The transient message surface (SPEC §7.5 "message / toast area").
//!
//! File and edit notices (opened, saved, save failed, edit rejected) surface here
//! as short-lived toasts in the top-right, **not** by hijacking the status bar's
//! position readout (the state this replaces). A toast is non-interactive: it paints
//! over the editor but consumes no input, so editing continues beneath it, and it
//! auto-fades after a TTL rather than waiting for a keystroke.
//!
//! Time is threaded in as an [`Instant`] argument (`push`/`expire` take `now`), so
//! the queue and expiry logic are unit-testable without a real clock (SPEC §13); the
//! I/O shell in `main.rs` passes `Instant::now()` and its 16ms poll tick drives the
//! fade even while the user is idle.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;
use vortex_core::Notification;

/// How long a toast stays before it fades.
const TTL: Duration = Duration::from_secs(4);
/// Most toasts shown at once; a burst drops the oldest so the stack never grows
/// down over the whole buffer.
const MAX: usize = 3;

/// A toast's severity, which picks its style. File/edit *failures* are errors (they
/// must stand out, SPEC §8); everything else is informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

/// The toast for a notification - message text and severity decided in one arm
/// per variant, so a future variant cannot get a message without also picking a
/// level (a failure silently rendered as a calm info toast is exactly what SPEC
/// §8's "a failed save must be visible" forbids). `None` for notifications the
/// frontend does not surface (e.g. `ShuttingDown`).
pub fn toast_for(note: &Notification) -> Option<(String, Level)> {
    use crate::layout::buffer_display_name;
    match note {
        Notification::FileOpened { path, existed, .. } => {
            let name = buffer_display_name(Some(path), false);
            let text = if *existed {
                format!("Opened {name}")
            } else {
                format!("{name} [New File]")
            };
            Some((text, Level::Info))
        }
        Notification::FileSaved { path, .. } => Some((
            format!("Saved {}", buffer_display_name(Some(path), false)),
            Level::Info,
        )),
        Notification::FileError { message, .. } => {
            Some((format!("Error: {message}"), Level::Error))
        }
        Notification::EditRejected { message, .. } => {
            Some((format!("Edit rejected: {message}"), Level::Error))
        }
        // Non-exhaustive: unknown/silent notifications surface no toast.
        _ => None,
    }
}

/// One transient message plus when it appeared.
struct Toast {
    text: String,
    level: Level,
    born: Instant,
}

/// The stack of live toasts, newest last. Owned by the event loop and rendered over
/// the editor each frame (SPEC §7.5).
pub struct Toasts {
    items: Vec<Toast>,
    info: Style,
    error: Style,
}

impl Toasts {
    /// A toast surface styled with the info/error styles from the theme.
    pub fn new(info: Style, error: Style) -> Self {
        Self {
            items: Vec::new(),
            info,
            error,
        }
    }

    /// Add a message stamped `now`, dropping the oldest if that would exceed [`MAX`]
    /// so a flood of notifications never fills the screen.
    pub fn push(&mut self, text: String, level: Level, now: Instant) {
        self.items.push(Toast {
            text,
            level,
            born: now,
        });
        if self.items.len() > MAX {
            self.items.remove(0);
        }
    }

    /// Drop toasts older than the TTL. Returns whether any were removed, so the
    /// caller repaints only when the surface actually changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items
            .retain(|t| now.saturating_duration_since(t.born) < TTL);
        self.items.len() != before
    }

    /// Paint the stack in the top-right corner, one padded line per toast, oldest at
    /// the top - just below the head bar (row 0). Right-aligned so it reads as chrome
    /// over the text rather than part of it; truncated to the screen width. Consumes
    /// no input: the editor keeps working beneath (SPEC §7.5).
    pub fn render(&self, screen: Rect, buf: &mut Buffer) {
        if self.items.is_empty() || screen.width == 0 {
            return;
        }
        for (i, toast) in self.items.iter().enumerate() {
            let y = screen.y + 1 + i as u16;
            if y >= screen.bottom() {
                break;
            }
            let style = match toast.level {
                Level::Info => self.info,
                Level::Error => self.error,
            };
            let label = format!(" {} ", toast.text);
            let w = (label.width() as u16).min(screen.width);
            let x = screen.right() - w;
            let rect = Rect::new(x, y, w, 1);
            buf.set_style(rect, style);
            buf.set_stringn(x, y, &label, w as usize, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use vortex_core::{BufferId, Notification};

    fn styles() -> (Style, Style) {
        (Style::new().bg(Color::Blue), Style::new().bg(Color::Red))
    }

    fn toasts() -> Toasts {
        let (info, error) = styles();
        Toasts::new(info, error)
    }

    #[test]
    fn toast_for_renders_file_events_with_their_severity() {
        use std::path::PathBuf;
        let id = BufferId(0);
        assert_eq!(
            toast_for(&Notification::FileOpened {
                buffer_id: id,
                path: PathBuf::from("dir/a.rs"),
                existed: true,
            }),
            Some(("Opened a.rs".into(), Level::Info))
        );
        assert_eq!(
            toast_for(&Notification::FileOpened {
                buffer_id: id,
                path: PathBuf::from("new.rs"),
                existed: false,
            }),
            Some(("new.rs [New File]".into(), Level::Info))
        );
        assert_eq!(
            toast_for(&Notification::FileSaved {
                buffer_id: id,
                path: PathBuf::from("dir/a.rs"),
            }),
            Some(("Saved a.rs".into(), Level::Info))
        );
        // Failures carry the error level in the same arm as their message.
        assert_eq!(
            toast_for(&Notification::FileError {
                buffer_id: id,
                path: None,
                message: "disk full".into(),
            }),
            Some(("Error: disk full".into(), Level::Error))
        );
        assert_eq!(
            toast_for(&Notification::EditRejected {
                buffer_id: id,
                version: 0,
                message: "nope".into(),
            }),
            Some(("Edit rejected: nope".into(), Level::Error))
        );
    }

    #[test]
    fn toast_for_none_for_shutting_down() {
        assert_eq!(toast_for(&Notification::ShuttingDown), None);
    }

    #[test]
    fn push_adds_a_toast() {
        let mut t = toasts();
        assert!(t.items.is_empty());
        t.push("hi".into(), Level::Info, Instant::now());
        assert_eq!(t.items.len(), 1);
    }

    #[test]
    fn push_caps_the_stack_dropping_the_oldest() {
        let mut t = toasts();
        let now = Instant::now();
        for i in 0..(MAX + 2) {
            t.push(format!("m{i}"), Level::Info, now);
        }
        assert_eq!(t.items.len(), MAX, "stack is capped");
        // The two oldest (m0, m1) were dropped; the newest survives.
        assert_eq!(t.items.first().unwrap().text, "m2");
        assert_eq!(t.items.last().unwrap().text, format!("m{}", MAX + 1));
    }

    #[test]
    fn expire_removes_only_toasts_past_the_ttl() {
        let mut t = toasts();
        let start = Instant::now();
        t.push("old".into(), Level::Info, start);
        t.push("fresh".into(), Level::Info, start + TTL); // 4s newer
        // At start + TTL + 1ms: "old" is past its TTL, "fresh" is not.
        let now = start + TTL + Duration::from_millis(1);
        assert!(t.expire(now), "something expired");
        assert_eq!(t.items.len(), 1);
        assert_eq!(t.items[0].text, "fresh");
        // Nothing left to expire yet -> no change, no repaint.
        assert!(!t.expire(now));
    }

    #[test]
    fn render_places_a_toast_top_right_with_its_level_style() {
        let mut t = toasts();
        t.push("saved".into(), Level::Info, Instant::now());
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf);
        // " saved " is 7 cells, flush to the right edge (cols 13..20) on row 1.
        let row1 = crate::testutil::row_text(&buf, 1);
        assert!(
            row1.ends_with(" saved "),
            "toast is right-aligned: {row1:?}"
        );
        // Row 0 (head bar) is untouched; the toast starts at row 1.
        assert_eq!(buf.cell((19, 0)).unwrap().bg, Color::Reset);
        // The toast cells carry the info style's background.
        assert_eq!(buf.cell((19, 1)).unwrap().bg, Color::Blue);
    }

    #[test]
    fn render_stacks_multiple_and_skips_when_empty() {
        let mut t = toasts();
        let area = Rect::new(0, 0, 20, 6);
        // Empty: render touches nothing.
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf);
        assert_eq!(buf, Buffer::empty(area));
        // Two toasts stack on rows 1 and 2.
        t.push("first".into(), Level::Info, Instant::now());
        t.push("second".into(), Level::Error, Instant::now());
        t.render(area, &mut buf);
        let row1 = crate::testutil::row_text(&buf, 1);
        let row2 = crate::testutil::row_text(&buf, 2);
        assert!(row1.contains("first"), "row1: {row1:?}");
        assert!(row2.contains("second"), "row2: {row2:?}");
        // The error toast uses the error background.
        assert_eq!(buf.cell((19, 2)).unwrap().bg, Color::Red);
    }
}
