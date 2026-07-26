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
use crate::view::BufferId;

/// The handful of settings that change what the *core* does, as opposed to how the
/// frontend paints (SPEC §10.5).
///
/// Configuration is frontend-owned and file-loaded, which leaves the few settings
/// that are genuinely about editing needing a way across. They come over the same
/// message seam as everything else rather than through a constructor argument,
/// because a remote frontend reads its user's config on *its* machine and has to be
/// able to send it - and because that keeps one path for a setting whether it
/// arrives at startup or changes mid-session.
///
/// Deliberately small. A setting belongs here only if the core is what acts on it:
/// tab width, key bindings and colors are frontend concerns and stay there
/// (SPEC §2.2, §5). The next candidate is §10.4's large-file degradation threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CoreOptions {
    /// Append a trailing newline when saving a buffer that lacks one - SPEC
    /// §10.1's POSIX-style default, and the reason a file edited here does not
    /// sprout a "\ No newline at end of file" in someone's diff. Only the bytes
    /// written are affected; the buffer is never touched, so this can never show
    /// up as an unsaved change.
    pub final_newline: bool,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Self {
            final_newline: true,
        }
    }
}

/// How much text a [`Action::SelectAround`] takes: the unit a repeated click grows
/// the selection by (SPEC §2.2).
///
/// A *unit of text*, not a screen measure, which is why the core resolves it: word
/// boundaries are Unicode segmentation and line bounds are the buffer's own, and
/// neither is something a frontend should be re-deriving from a rendered row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Granularity {
    /// The word under the offset - or, off a word, the run of whitespace or
    /// punctuation under it, so a double-click always selects *something*.
    Word,
    /// The whole line including its terminator, so selecting one and deleting it
    /// removes the line rather than leaving a blank one behind.
    Line,
}

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
    /// Select the whole word or line at `offset`, replacing the selection set with
    /// that one span (the double- and triple-click gestures, and a click in the line
    /// number gutter).
    ///
    /// The frontend resolves the pointer to an offset, as it does for
    /// [`Action::PlaceCursor`] - it owns display↔buffer mapping (SPEC §4/§5) - and
    /// the *core* resolves the offset to a range, because where a word ends is a
    /// question about text, not about the screen. `offset` is clamped to the buffer
    /// defensively (SPEC §8); the anchor lands at the range's start and the head at
    /// its end, so a subsequent shift-click extends from the far side as expected.
    ///
    /// Changes only the selection set: no text, so no delta and no version bump.
    SelectAround {
        offset: usize,
        granularity: Granularity,
    },
    /// Insert `text` at every selection, replacing any non-empty selection first.
    /// A bracketed paste is ONE such action, not a key-per-character (SPEC §6).
    Insert(String),
    /// Delete the grapheme before each cursor (Backspace), or the selected text
    /// if the selection is non-empty.
    DeleteBackward,
    /// Delete the grapheme after each cursor (Delete), or the selected text if
    /// the selection is non-empty.
    DeleteForward,
    /// Copy each non-empty selection's text into the clipboard register (SPEC §11),
    /// one entry per selection in selection order. The buffer is unchanged (no delta
    /// or version bump); the core emits a `Notification::SetClipboard` so the
    /// frontend can mirror the register to the OS clipboard (OSC 52 / native). A
    /// set of bare cursors (nothing selected) is a no-op.
    Copy,
    /// Copy the selections into the register (as [`Action::Copy`]) and then delete
    /// them - one edit, one undo unit. A set of bare cursors is a no-op.
    Cut,
    /// Insert the clipboard register at every cursor (SPEC §11), replacing any
    /// non-empty selection first. Distribution: when the register holds exactly one
    /// entry it is inserted at every cursor; when it holds exactly as many entries
    /// as there are cursors, the i-th entry goes to the i-th cursor (the multi-cursor
    /// copy round-trip); otherwise the entries are joined with newlines and that one
    /// block is inserted at every cursor. An empty register is a no-op. Like
    /// [`Action::Insert`] this is ONE action, not a key-per-character (SPEC §6).
    Paste,
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
    /// Open `path` and make it the active buffer (SPEC §12.2 file lifecycle).
    ///
    /// If `path` is already open this switches to that buffer instead of loading a
    /// second copy - two buffers over one file would each carry their own history
    /// and could overwrite each other's saves. Otherwise the file loads into the
    /// active buffer when it is an untouched scratch (unnamed, empty, unmodified),
    /// so launching bare and then opening does not strand an empty tab, and into a
    /// newly created buffer when it is not.
    ///
    /// A missing file is not an error: it opens an empty buffer bound to `path`,
    /// created on the first `Save` (Vim's behavior). A load is expressed as one
    /// `Delta` replacing the whole (empty or reused) buffer, so the delta/snapshot
    /// invariant (SPEC §5) still holds.
    Open(PathBuf),
    /// Write the buffer to its associated file (set by `Open`). Fails with a
    /// `Notification` if no path is set - write to an explicit target with
    /// [`Action::SaveAs`] instead. The write is atomic (temp file + rename, SPEC §8)
    /// so a failed write never corrupts the existing file, and the buffer stays
    /// dirty on failure so no work is lost.
    Save,
    /// Write the buffer to `path` and adopt it as the buffer's file, so subsequent
    /// `Save`s target it (the "save-as" the frontend's prompt commits, SPEC §7.5).
    /// The write is atomic like `Save`; on failure the old path binding and dirty
    /// state are left untouched (SPEC §8), so a rejected save-as loses neither the
    /// work nor the original association. Adopting a new path re-announces the
    /// document to the language server (a fresh `didOpen`, possibly a new
    /// `languageId`) since its identity changed.
    SaveAs(PathBuf),
    /// Make `id` the active buffer - the one every other action applies to. A no-op
    /// for an unknown id or the already-active buffer. Switching is pure state: no
    /// text changes, so no delta and no version bump, but the snapshot that follows
    /// describes a different buffer entirely.
    ///
    /// Next/previous buffer are deliberately *not* actions: the frontend already
    /// holds the ordered buffer list to paint its bufferline, so it resolves the
    /// neighbor itself and sends this. The core's vocabulary stays intent about
    /// *which* buffer, not how the user got there.
    SwitchBuffer { id: BufferId },
    /// Close buffer `id`. Refused with a `Notification::CloseRejected` when the
    /// buffer has unsaved edits, unless `force` (SPEC §8: work is never discarded
    /// without the user saying so, and the *core* enforces that so a non-terminal
    /// frontend cannot skip the check). Closing the active buffer moves focus to a
    /// neighbor; closing the last one leaves a fresh empty buffer, since a session
    /// always has somewhere to type.
    CloseBuffer { id: BufferId, force: bool },
    /// Re-read buffer `id` from its file, discarding the buffer's own contents
    /// (SPEC §10.2). Sent when the user resolves the conflict a
    /// `Notification::ExternalChange` raised by taking the disk side.
    ///
    /// Refused with a `Notification::ReloadRejected` when the buffer has unsaved
    /// edits, unless `force` - the same guard `CloseBuffer` carries, for the same
    /// reason: a reload discards work just as thoroughly as a close, so the *core*
    /// holds the check rather than trusting every frontend to ask first (SPEC §8).
    /// A buffer with no file bound, or an unknown id, is a no-op.
    ///
    /// Selections are clamped to the new text rather than reset: the file usually
    /// changed somewhere other than where the user was looking, and sending the
    /// caret home would be its own kind of lost work.
    Reload { id: BufferId, force: bool },
    /// Replace the core's settings with `options` (SPEC §10.5). Sent once at
    /// startup with whatever the frontend's config file resolved to, and again if
    /// it is ever reloaded; nothing is merged, so the value sent is the value in
    /// force. Changes no text, so no delta and no version bump.
    Configure(CoreOptions),
    /// Shut the editor down cleanly. The core drains and stops its loop.
    Quit,
}
