//! `Action` - intent sent from a frontend into the core (SPEC §1, §12.2).
//!
//! Actions model *intent* (`MoveCursor(Right)`), never keystrokes (`Ctrl+Right`).
//! Key->intent translation is the frontend's job, so a future GUI with different
//! keys emits the same actions. M1 defines motion + edit + snapshot/quit; the
//! rest of the vocabulary (selection ops, history, file lifecycle) lands M3+.
//!
//! `Action` derives `Serialize`/`Deserialize` from the start (SPEC §8.1): the
//! action journal and the future remote-frontend wire both need it, and deriving
//! it now means they ride along for free instead of forcing a later retrofit.

use serde::{Deserialize, Serialize};

use crate::selection::Motion;

/// A single intent from a frontend to the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Action {
    /// Move every selection by `motion`. `extend` grows selections (holding their
    /// anchor); otherwise each collapses to a cursor at the new head (SPEC §2.2).
    MoveCursor { motion: Motion, extend: bool },
    /// Insert `text` at every selection, replacing any non-empty selection first.
    /// A bracketed paste is ONE such action, not a key-per-character (SPEC §6).
    Insert(String),
    /// Delete the grapheme before each cursor (Backspace), or the selected text
    /// if the selection is non-empty.
    DeleteBackward,
    /// Delete the grapheme after each cursor (Delete), or the selected text if
    /// the selection is non-empty.
    DeleteForward,
    /// Request an immediate `ViewSnapshot` without changing state.
    RequestSnapshot,
    /// Shut the editor down cleanly. The core drains and stops its loop.
    Quit,
}
