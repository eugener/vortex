//! `Action` - intent sent from a frontend into the core (SPEC §1, §12.2).
//!
//! Actions model *intent* (`MoveCursor(Right)`), never keystrokes (`Ctrl+Right`).
//! Key->intent translation is the frontend's job, so a future GUI with different
//! keys emits the same actions. Motion + edit + snapshot/quit + file open/save
//! are defined; the rest of the vocabulary (selection ops, history) lands M3+.
//!
//! `Action` derives `Serialize`/`Deserialize` from the start (SPEC §8.1): the
//! action journal and the future remote-frontend wire both need it, and deriving
//! it now means they ride along for free instead of forcing a later retrofit.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::selection::Motion;

/// A single intent from a frontend to the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Action {
    /// Move every selection by `motion`. `extend` grows selections (holding their
    /// anchor); otherwise each collapses to a cursor at the new head (SPEC §2.2).
    MoveCursor { motion: Motion, extend: bool },
    /// Place the caret at absolute byte `offset`, collapsing to a single selection
    /// (a pointer click). The frontend resolves the pointer's screen cell to a
    /// buffer offset - it owns display<->buffer mapping (SPEC §4/§5), so the core
    /// receives intent ("caret here"), not raw coordinates. `extend` keeps the
    /// current primary anchor and moves only the head (drag / shift-click) so a
    /// drag grows a selection; otherwise the set becomes a plain cursor at `offset`.
    /// `offset` is clamped to the buffer defensively (SPEC §8).
    PlaceCursor { offset: usize, extend: bool },
    /// Add a cursor one line above the topmost caret at its column, keeping the
    /// existing cursors (the column-select gesture, SPEC §2.2). A no-op at the first
    /// line. Changes only the selection set: no text, so no delta or version bump.
    AddCursorAbove,
    /// Add a cursor one line below the bottommost caret at its column (SPEC §2.2).
    /// A no-op at the last line.
    AddCursorBelow,
    /// Add a plain cursor at absolute byte `offset` (a modifier-click), keeping the
    /// existing cursors (SPEC §2.2). Like [`Action::PlaceCursor`] the frontend
    /// resolves the pointer to an offset; `offset` is clamped to the buffer (SPEC §8).
    AddCursorAt { offset: usize },
    /// Collapse a multi-cursor set back to the primary selection alone (Escape,
    /// SPEC §2.2). The primary keeps its span; the rest are dropped.
    CollapseSelections,
    /// Insert `text` at every selection, replacing any non-empty selection first.
    /// A bracketed paste is ONE such action, not a key-per-character (SPEC §6).
    Insert(String),
    /// Delete the grapheme before each cursor (Backspace), or the selected text
    /// if the selection is non-empty.
    DeleteBackward,
    /// Delete the grapheme after each cursor (Delete), or the selected text if
    /// the selection is non-empty.
    DeleteForward,
    /// Undo the most recent edit (SPEC §2.4). Moves the buffer to the current
    /// history node's parent, restoring the pre-edit text and selections. A no-op
    /// at the root. Works for any edit action - insert, delete, paste, multi-cursor
    /// - because history records buffer changes, not action kinds.
    Undo,
    /// Redo the edit undone most recently, following the newest branch of the undo
    /// tree (SPEC §2.4). A no-op when there is nothing to redo on the current branch.
    Redo,
    /// Request an immediate `ViewSnapshot` without changing state.
    RequestSnapshot,
    /// Replace the buffer with the contents of `path` and remember it as the
    /// buffer's file (SPEC §12.2 file lifecycle). A missing file is not an error:
    /// it opens an empty buffer bound to `path`, created on the first `Save`
    /// (Vim's behavior). The load is expressed as one `Delta` replacing the whole
    /// buffer, so the delta/snapshot invariant (SPEC §5) still holds.
    Open(PathBuf),
    /// Write the buffer to its associated file (set by `Open`). Fails with a
    /// `Notification` if no path is set - save-as (a target path) lands with the
    /// prompt UI, not here. The write is atomic (temp file + rename, SPEC §8) so
    /// a failed write never corrupts the existing file, and the buffer stays
    /// dirty on failure so no work is lost.
    Save,
    /// Shut the editor down cleanly. The core drains and stops its loop.
    Quit,
}
