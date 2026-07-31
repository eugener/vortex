//! Core -> frontend messages: `Delta` (authoritative change log), `ViewSnapshot`
//! (derived local render state), and `Notification` (discrete events). See SPEC
//! §5 (render model) and §6 (channels).
//!
//! **`Delta` is primary; the snapshot is derived** (SPEC §5). An edit *is* a
//! `Delta { range, new_text }` before it touches the buffer, and the core is
//! already committed to producing that value for the undo tree, LSP `didChange`,
//! and partial repaint - so one representation of change unifies all of them plus
//! remote sync and the journal. The snapshot is the cheap `Arc` bundle a *local*
//! frontend paints from without replaying deltas; a remote frontend consumes the
//! delta stream and never receives a whole-buffer snapshot.
//!
//! Serialization split (SPEC §5 seam-cost note): `Delta` and `Notification` derive
//! `Serialize`/`Deserialize` - they are small value messages that become the wire
//! protocol essentially for free. `ViewSnapshot` carries the whole rope (`Text`),
//! does NOT serialize cheaply, and never needs to: it is local-only.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::buffer::Text;
use crate::decoration::DecorationSet;
use crate::selection::Selection;

/// Identifies a buffer. Versions are per-buffer (SPEC §5), so an edit in one
/// buffer never invalidates another's anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BufferId(pub u64);

/// One entry in the snapshot's list of open buffers - what a bufferline or buffer
/// picker paints, for the buffers that are *not* on screen (SPEC §7.5).
///
/// Carried on [`ViewSnapshot`] rather than fetched, for the same reason its `path`
/// and `modified` are: a local frontend paints the whole strip with zero round
/// trips (SPEC §5). The core rebuilds this list only when it actually changes, so
/// an edit does not re-clone every open buffer's path per keystroke.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferInfo {
    pub id: BufferId,
    /// The file this buffer is bound to, or `None` for an unnamed buffer.
    pub path: Option<PathBuf>,
    /// Whether it has unsaved edits - the modified marker on its tab.
    pub modified: bool,
}

/// The authoritative "what changed" message: replace `range` (byte offsets in the
/// pre-edit buffer) with `new_text` (SPEC §5). This is the exact shape of the
/// buffer's edit primitive, and applying the delta stream from version N to a
/// version-N buffer must reproduce the version-(N+1) buffer - the property tested
/// in §13. Small and serializable: this is the remote wire protocol and journal
/// record, not a whole-buffer dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delta {
    /// Which buffer this change applies to.
    pub buffer_id: BufferId,
    /// The buffer version this delta advances *from*. A frontend applies it only
    /// to a buffer currently at `base_version` (SPEC §5 ordering guarantee).
    pub base_version: u64,
    /// Byte range in the pre-edit (base_version) buffer to replace.
    pub range: std::ops::Range<usize>,
    /// Replacement text. Empty for a pure deletion.
    pub new_text: String,
}

/// Immutable render state a *local* frontend paints from - a derived convenience,
/// not the authoritative change log (that is [`Delta`], SPEC §5). Latest-wins: the
/// frontend only ever needs the newest (SPEC §5, §6).
///
/// Every field is cheaply shared (SPEC §5): `text` is an `Arc`-backed rope handle
/// and `selections` is behind `Arc`, so building a snapshot is a handful of
/// atomic ref-count bumps regardless of file size or selection count - never an
/// O(n) deep clone per frame - `decorations` included, which is why it is an
/// `Arc<DecorationSet>` and not a `Vec` of spans.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ViewSnapshot {
    pub buffer_id: BufferId,
    /// Per-buffer monotonic counter; the frontend ignores snapshots older than
    /// the newest it holds. Advances on edits, not on snapshot requests.
    pub version: u64,
    /// The buffer contents at `version` - a cheap `Arc` clone (SPEC §5).
    pub text: Text,
    /// Selections resolved to concrete byte positions at `version`, `Arc`-shared.
    pub selections: Arc<[Selection]>,
    /// Index into `selections` of the primary selection - the one that drives
    /// viewport-follow and prompts (SPEC §2.2). Always a valid index (the set is
    /// never empty). Carrying it means the frontend follows the primary caret
    /// rather than guessing `selections[0]`, which diverges once M3 multi-cursor
    /// makes the primary != index 0.
    pub primary: usize,
    /// The byte range that changed since the previous version, if this snapshot
    /// followed an edit. `None` for a snapshot produced without an edit (e.g. a
    /// `RequestSnapshot`). A local frontend uses it as a partial-repaint hint
    /// (SPEC §5); painting the whole viewport is always correct if ignored.
    pub dirty: Option<std::ops::Range<usize>>,
    /// Everything to paint *at a position*: LSP diagnostics now, syntax
    /// highlights (M4) and git signs (M8) later, all on one channel (SPEC §5).
    /// `Arc`-shared, so carrying it costs a ref-count bump no matter how many
    /// spans it holds. The frontend resolves it for the **visible line range
    /// only** (`underlines_in` / `gutter_mark`), never eagerly for the file.
    ///
    /// May lag `text` by a frame: producers are async, and an edit remaps
    /// existing decorations rather than blocking on a refresh. That is the
    /// correct trade - text is never stale, overlays only trail (SPEC §5).
    pub decorations: Arc<DecorationSet>,
    /// The file this buffer is bound to (via `Open`), or `None` for an unnamed
    /// buffer. The frontend shows it in the status/head bar (SPEC §10). Carried
    /// on the snapshot rather than queried so a local frontend paints the name
    /// with zero round-trips (SPEC §5).
    pub path: Option<PathBuf>,
    /// Whether the buffer has unsaved edits (differs from its on-disk file).
    /// A distinct axis from `version` (buffer identity for anchors/LSP) and
    /// `dirty` (the repaint hint): this is purely "is there unsaved work". The
    /// frontend paints a modified marker from it (SPEC §8, §10).
    pub modified: bool,
    /// How the active buffer's file is stored on disk - encoding, BOM, line
    /// terminator (SPEC §10.1). Detected on load and reproduced on save; the
    /// frontend shows it in the status bar, which is the only way a user can tell
    /// that opening a CRLF latin-1 file did not quietly convert it. `Copy` and two
    /// words wide, so carrying it per frame costs nothing.
    pub format: crate::file::FileFormat,
    /// Whether the buffer refuses edits (SPEC §10.3): the file cannot be written,
    /// or parts of it did not decode and saving would overwrite them. The frontend
    /// marks it in the status bar; the core is what actually enforces it, so a
    /// frontend that ignores this cannot write to the file anyway.
    pub read_only: bool,
    /// Every open buffer, in the order a bufferline lists them, including the
    /// active one (identified by `buffer_id`). `Arc`-shared and rebuilt only when
    /// the set actually changes, so carrying it costs a ref-count bump per frame
    /// (SPEC §5). Never empty: a session always holds at least one buffer.
    pub buffers: Arc<[BufferInfo]>,
}

/// Discrete core -> frontend events (errors, status, prompts). Self-contained on
/// purpose: a notification may arrive out of order with snapshots, so it carries
/// the `buffer_id`/`version` it refers to rather than assuming a paired snapshot
/// is present (SPEC §6). Serializable for the remote seam and journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Notification {
    /// An edit was rejected (bad range, read-only, etc.); state is unchanged.
    /// Carries the buffer + version it was evaluated against (SPEC §6, §8).
    EditRejected {
        buffer_id: BufferId,
        version: u64,
        message: String,
    },
    /// A file was loaded into `buffer_id` from `path`. `existed` is false when
    /// the path did not exist (a fresh empty buffer bound to it, created on the
    /// first save). Self-contained per SPEC §6: carries the path, not a promise
    /// that a paired snapshot is present.
    FileOpened {
        buffer_id: BufferId,
        path: PathBuf,
        existed: bool,
    },
    /// The buffer was written to `path`. The buffer is now clean.
    FileSaved { buffer_id: BufferId, path: PathBuf },
    /// A file open or save failed; buffer state is unchanged and (for a failed
    /// save) still dirty, so no work is lost (SPEC §8). Carries a human-readable
    /// reason; `path` is `None` for "save with no file bound".
    FileError {
        buffer_id: BufferId,
        path: Option<PathBuf>,
        message: String,
    },
    /// The clipboard register changed (a `Copy`/`Cut`), carrying the flattened
    /// text the frontend should push to the OS clipboard (SPEC §11). Flattened
    /// because the OS clipboard is a single string: the per-selection register
    /// entries are joined with newlines here, while the structured register stays
    /// in the core for a `Paste` round-trip. Serializable so it rides the remote
    /// seam too (a remote frontend bridges to *its* clipboard).
    SetClipboard { text: String },
    /// The active buffer changed (an `Open` of an already-open file, a
    /// `SwitchBuffer`, or the buffer that inherited focus after a close). Carries
    /// the newly active buffer's path so the frontend can attach the right language
    /// server and grammar for it, the same way it does off `FileOpened` - a switch
    /// changes the *language* on screen just as much as an open does.
    BufferSwitched {
        buffer_id: BufferId,
        path: Option<PathBuf>,
    },
    /// A buffer was closed and is gone from the snapshot's buffer list.
    BufferClosed { buffer_id: BufferId },
    /// The file behind a clean buffer changed on disk and the buffer was brought
    /// up to date with it (SPEC §8). There was no unsaved work to weigh against
    /// it, so the reload is silent rather than a question - the frontend shows it
    /// as status, not a prompt.
    FileReloaded { buffer_id: BufferId, path: PathBuf },
    /// The file behind a buffer changed on disk and the core did **not** act on
    /// it, so the user must (SPEC §8: never silently overwrite either side).
    ///
    /// Either the buffer has unsaved edits - reloading would discard them, keeping
    /// them would discard the file's - or the file is gone. The frontend prompts;
    /// `Action::Reload` takes the disk side, and doing nothing keeps the buffer.
    ExternalChange {
        buffer_id: BufferId,
        path: PathBuf,
        /// The file no longer exists. Nothing to reload from: the buffer is the
        /// only copy left, and saving it writes the file back.
        removed: bool,
    },
    /// A reload was refused because the buffer has unsaved edits, the twin of
    /// [`Notification::CloseRejected`] and for the same reason: discarding work
    /// takes the user saying so, and the *core* is where that cannot be skipped.
    ReloadRejected { buffer_id: BufferId, path: PathBuf },
    /// A save was refused because the file changed underneath the buffer since it
    /// was last read or written (SPEC §8, §10.2). The third of the same family:
    /// writing over someone else's change destroys work exactly as reloading or
    /// closing over your own does, so it takes the user saying so.
    ///
    /// Normally the watcher has already raised
    /// [`Notification::ExternalChange`] and the user has answered it. This fires
    /// when it did not - a dropped event, a backend that missed the write, a
    /// frontend running no watcher - which is why the check is at the write and not
    /// only on the notification path. The frontend confirms and re-sends
    /// `Action::Save { force: true }`.
    SaveRejected {
        buffer_id: BufferId,
        path: PathBuf,
        /// The file is *gone* rather than modified. Saving recreates it, which is
        /// usually what the user wants - but it is still their call, since the file
        /// may have been deleted deliberately.
        removed: bool,
    },
    /// A close was refused because the buffer has unsaved edits (SPEC §8: never
    /// silently lose work). The frontend confirms with the user and, if they accept,
    /// re-sends the close with `force`. Carries the path so the prompt can name the
    /// file it is about to discard.
    CloseRejected {
        buffer_id: BufferId,
        path: Option<PathBuf>,
    },
    /// A search did not happen: the pattern is not a valid regex, or it matches
    /// nowhere in the buffer (SPEC §11). Nothing changed either way.
    ///
    /// Both are the same kind of event to a frontend - the keypress did nothing and
    /// the user is owed a reason - so they are one variant carrying the reason rather
    /// than two the frontend would render identically. A pattern is user input, so
    /// this is a notification and never a panic (SPEC §8); `matched` tells a valid
    /// pattern that found nothing (`true`, worth a quiet "no matches") apart from one
    /// that could not be compiled (`false`, worth showing the engine's complaint).
    SearchFailed {
        buffer_id: BufferId,
        /// The pattern as sent, so a toast can quote what found nothing.
        pattern: String,
        /// `regex`'s compile error, or a "no matches" message.
        message: String,
        /// Whether the pattern compiled. `false` means `message` is a syntax error.
        compiled: bool,
    },
    /// The core has stopped its loop and will send nothing further.
    ShuttingDown,
}
