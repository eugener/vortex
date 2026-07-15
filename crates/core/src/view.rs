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
use crate::selection::Selection;

/// Identifies a buffer. Versions are per-buffer (SPEC §5), so an edit in one
/// buffer never invalidates another's anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BufferId(pub u64);

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
/// O(n) deep clone per frame. `styles` (tree-sitter/LSP) joins this in M4.
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
    /// The core has stopped its loop and will send nothing further.
    ShuttingDown,
}
