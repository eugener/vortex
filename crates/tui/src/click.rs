//! Counting a click as single, double, or triple (SPEC §2.2's pointer gestures).
//!
//! Terminals report presses, not gestures: there is no double-click event in the
//! SGR mouse protocol, so the frontend times and counts them itself. That makes
//! this the one piece of mouse handling with *state*, which is why it lives in its
//! own module rather than as two more variables in the event loop - and why time
//! arrives as an [`Instant`] argument, so the counting is testable without a clock
//! (the same shape [`crate::toast`] uses).
//!
//! What a count means is the caller's business: today one places the caret, two
//! selects a word, three selects a line.

use std::time::{Duration, Instant};

/// How long after a press a second one still reads as part of the same gesture.
/// Matches the ~400-500ms most desktop environments use; short enough that two
/// deliberate clicks in the same spot are not accidentally joined, long enough not
/// to punish a slow double-click.
const WINDOW: Duration = Duration::from_millis(400);

/// The most clicks a gesture counts to before starting over.
const MAX: usize = 3;

/// Tracks consecutive presses so a repeat can be told from a fresh click.
#[derive(Debug, Default)]
pub struct Clicks {
    /// When and where the last press landed. `None` before the first one.
    last: Option<(Instant, u16, u16)>,
    /// How many presses the current run has reached, 1..=[`MAX`].
    count: usize,
}

impl Clicks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a press at a cell and return what it counts as: 1, 2, or 3.
    ///
    /// A press continues the run when it lands in the **same cell** within
    /// [`WINDOW`], and starts a new one otherwise. Same *cell*, not "within a few
    /// pixels": a terminal cell is already the resolution the pointer reports at,
    /// so anything looser would join two clicks a user aimed at different
    /// characters.
    ///
    /// The run wraps back to 1 after [`MAX`], so a fourth click starts over rather
    /// than sticking on "line" - a user who keeps clicking is trying to get
    /// somewhere, and the alternative is a gesture with no way back.
    pub fn press(&mut self, column: u16, row: u16, now: Instant) -> usize {
        let repeat = self.last.is_some_and(|(when, c, r)| {
            c == column && r == row && now.saturating_duration_since(when) < WINDOW
        });
        self.count = if repeat && self.count < MAX {
            self.count + 1
        } else {
            1
        };
        self.last = Some((now, column, row));
        self.count
    }

    /// Forget the current run, so the next press counts as a first one.
    ///
    /// For the gestures that are not part of the sequence at all - an Alt-click
    /// adding a cursor, a drag - which would otherwise leave a stale run for an
    /// unrelated click to continue.
    pub fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }
}

#[cfg(test)]
#[path = "click_tests.rs"]
mod tests;
