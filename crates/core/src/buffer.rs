//! The text buffer, behind a `Buffer` trait (SPEC §2.1, §10.4).
//!
//! `crop::Rope` never appears in this module's public surface: the buffer sits
//! behind the [`Buffer`] trait and text is exposed only as the opaque [`Text`]
//! handle. That is the compile-time guarantee that keeps two future backends
//! swap-ready without touching call sites - a CRDT (§11) and a Tier-3 paged/mmap
//! buffer for bigger-than-RAM files (§10.4) - because nothing downstream can
//! depend on the rope being the storage.
//!
//! The one edit primitive is [`Buffer::replace`]: `replace(range, "")` deletes,
//! `replace(n..n, s)` inserts. It is shaped as a byte range plus replacement text
//! on purpose - that is exactly a `Delta` (SPEC §5), so one representation of
//! change later unifies undo, LSP sync, and partial repaint instead of separate
//! insert/delete paths.
//!
//! Coordinate spaces are named per SPEC §4: this module deals in **byte offsets**
//! (canonical storage) and **line/column** (derived, for "go to line" and
//! selections). Grapheme motion lives in `selection`; display columns live in the
//! frontend.

use std::ops::Range;

use crop::Rope;

/// A line/column position (SPEC §4). `line` is a 0-based line index; `col` is a
/// 0-based **byte** offset within that line's text (its line terminator
/// excluded). Byte columns keep this conversion cheap and lossless; grapheme
/// columns for cursor motion are derived in `selection` against the line's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Immutable, cheaply-cloneable text handle exposed by the buffer.
///
/// Wraps `crop::Rope` (which shares data via `Arc`, so cloning is "extremely
/// cheap" - verified, SPEC §3) but exposes only read methods, so `crop` never
/// leaks into a public signature (SPEC §2.1). A [`ViewSnapshot`](crate::view)
/// carries a `Text` and cloning one to hand to the frontend and to a background
/// reparse is a handful of atomic ref-count bumps regardless of file size.
#[derive(Debug, Clone, Default)]
pub struct Text(Rope);

impl Text {
    /// Total length in UTF-8 bytes.
    pub fn byte_len(&self) -> usize {
        self.0.byte_len()
    }

    /// True if the buffer holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.byte_len() == 0
    }

    /// Number of lines. A final line break is not counted as a separate empty
    /// line (crop semantics), so `"a\nb\n"` is 2 lines and `""` is 0.
    pub fn line_count(&self) -> usize {
        self.0.line_len()
    }

    /// The text of `line_index`, without its line terminator. Returns `None` if
    /// the index is out of range. Allocates only the requested line, never the
    /// whole buffer - keeps rendering viewport-bounded (SPEC §10.4).
    pub fn line(&self, line_index: usize) -> Option<String> {
        if line_index >= self.0.line_len() {
            return None;
        }
        Some(self.0.line(line_index).to_string())
    }

    /// Byte length of `line_index`'s content, without its terminator. Returns
    /// `None` if out of range. Reads crop's `RopeSlice::byte_len()` directly, so
    /// it never allocates the line's text (unlike [`Self::line`]) - the cheap path
    /// for "how long is this line" on the motion hot path (SPEC §10.4).
    pub fn line_len(&self, line_index: usize) -> Option<usize> {
        if line_index >= self.0.line_len() {
            return None;
        }
        Some(self.0.line(line_index).byte_len())
    }

    /// Byte offset at which `line_index` starts. Returns `None` if out of range.
    /// A trailing offset equal to `byte_len()` is valid for `line_count()` only
    /// when the buffer ends without a newline; callers use [`Self::line`] bounds
    /// for iteration rather than assuming.
    pub fn byte_of_line(&self, line_index: usize) -> Option<usize> {
        if line_index > self.0.line_len() {
            return None;
        }
        Some(self.0.byte_of_line(line_index))
    }

    /// Line index containing `byte_offset`. `byte_offset == byte_len()` maps to
    /// the last line. O(log n) via crop's internal index - never a scan.
    pub fn line_of_byte(&self, byte_offset: usize) -> usize {
        self.0.line_of_byte(byte_offset.min(self.0.byte_len()))
    }

    /// Line/column position of `byte_offset`, clamped to the buffer end. The one
    /// home for byte -> (line, col): the [`Buffer`] impl, cursor motion, and the
    /// frontend's cursor readout all delegate here, so the clamping rule cannot
    /// drift between call sites.
    pub fn position_of_byte(&self, byte_offset: usize) -> Position {
        let offset = byte_offset.min(self.0.byte_len());
        let line = self.0.line_of_byte(offset);
        Position::new(line, offset - self.0.byte_of_line(line))
    }

    /// Index of the last line a cursor can occupy. For a newline-terminated
    /// buffer that is the virtual empty line *after* the final terminator, which
    /// [`Self::line_count`]'s storage semantics do not count ("a\n" is 1 line to
    /// crop but the cursor can reach line 1). Shared by vertical motion in the
    /// core and the frontend's display line count so the two sides of the seam
    /// can never disagree on the navigable range.
    pub fn last_line_index(&self) -> usize {
        self.0.line_of_byte(self.0.byte_len())
    }

    /// The text of byte `range` as a `String` - used to copy a selection into the
    /// clipboard register (SPEC §11). Endpoints are clamped to the buffer and must
    /// land on code-point boundaries; a non-boundary or inverted range yields `""`
    /// rather than panicking (crop's `byte_slice` panics off a boundary), keeping
    /// this off the panic path (SPEC §8). Selection endpoints are always valid
    /// boundaries in practice, so the guard is defensive, not a hot branch.
    pub fn slice(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.0.byte_len());
        let start = range.start.min(end);
        if !self.0.is_char_boundary(start) || !self.0.is_char_boundary(end) {
            return String::new();
        }
        self.0.byte_slice(start..end).to_string()
    }
}

/// The full contents as a `String` via `to_string()`. O(n) - for tests and save
/// paths, never the render hot path (the frontend reads the visible line range).
impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A byte offset range within a buffer (SPEC §4: canonical storage space).
pub type ByteRange = Range<usize>;

/// The editable buffer abstraction. Backends (`crop` today; CRDT / paged later)
/// implement this; the rest of the core talks only to the trait (SPEC §2.1).
pub trait Buffer {
    /// A cheap immutable handle to the current contents (SPEC §5).
    fn text(&self) -> Text;

    /// Total length in UTF-8 bytes.
    fn byte_len(&self) -> usize;

    /// Replace `range` with `text`. The single edit primitive: delete is an empty
    /// `text`, insert is an empty `range`. This is the shape of a `Delta`
    /// (SPEC §5).
    ///
    /// Returns the text that was removed (the content of `range` before the
    /// replacement). That is the inverse information an undo needs, captured at
    /// the one layer that can slice it safely - the range is validated here, so
    /// the endpoints are known code-point boundaries - and it later feeds LSP
    /// `didChange`/the journal too (SPEC §5: one representation of change).
    ///
    /// # Errors
    /// Returns [`EditError`] if the range is out of bounds or not on a UTF-8 code
    /// point boundary - never panics on bad input (SPEC §8).
    fn replace(&mut self, range: ByteRange, text: &str) -> Result<String, EditError>;

    /// Convert a byte offset to a line/column position. Clamps offsets past the
    /// end to the buffer end rather than erroring (a derived read, SPEC §4).
    fn position_of_byte(&self, byte_offset: usize) -> Position;

    /// Convert a line/column position to a byte offset. Returns `None` if the
    /// line is out of range or the column exceeds that line's byte length.
    fn byte_of_position(&self, pos: Position) -> Option<usize>;
}

/// Why an edit was rejected. Typed so the seam can surface it as a
/// `Notification::Error` (SPEC §8) instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// The range end exceeds the buffer length.
    #[error("edit range {start}..{end} out of bounds (buffer is {len} bytes)")]
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    /// The range start is greater than its end.
    #[error("edit range start {start} is past its end {end}")]
    Inverted { start: usize, end: usize },
    /// A range endpoint fell in the middle of a UTF-8 code point.
    #[error("edit range endpoint {offset} is not on a UTF-8 code point boundary")]
    NotCharBoundary { offset: usize },
}

/// The `crop`-backed [`Buffer`]. The only place `crop::Rope` is named as storage;
/// swapping backends means another `impl Buffer`, not edits across the core.
#[derive(Debug, Clone, Default)]
pub struct RopeBuffer {
    rope: Rope,
}

/// Build a buffer from initial text. Loading is O(n) to build the rope, but does
/// no *extra* full-file scan for line indexing - crop builds its line index as
/// part of construction, satisfying the "no eager scan" invariant (SPEC §10.4).
/// Encoding/EOL detection (M5) will sample rather than scan.
impl From<&str> for RopeBuffer {
    fn from(text: &str) -> Self {
        Self {
            rope: Rope::from(text),
        }
    }
}

impl RopeBuffer {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate a range against the current rope: in-bounds, non-inverted, and
    /// both endpoints on code point boundaries. Shared by every edit path.
    fn validate(&self, range: &ByteRange) -> Result<(), EditError> {
        let len = self.rope.byte_len();
        if range.start > range.end {
            return Err(EditError::Inverted {
                start: range.start,
                end: range.end,
            });
        }
        if range.end > len {
            return Err(EditError::OutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        // Endpoints must land on char boundaries; check against the rope's bytes
        // so we reject e.g. slicing a multi-byte char in half (crop would panic).
        for &offset in &[range.start, range.end] {
            if !self.is_char_boundary(offset) {
                return Err(EditError::NotCharBoundary { offset });
            }
        }
        Ok(())
    }

    /// Whether `offset` lies on a UTF-8 code point boundary within the rope.
    /// An offset past the end reports `false` - crop's own `is_char_boundary`
    /// panics there (its one out-of-bounds case), so the guard keeps this off
    /// the panic path (SPEC §8). O(log n), zero-alloc: crop locates the chunk
    /// and defers to `str::is_char_boundary`.
    fn is_char_boundary(&self, offset: usize) -> bool {
        offset <= self.rope.byte_len() && self.rope.is_char_boundary(offset)
    }
}

impl Buffer for RopeBuffer {
    fn text(&self) -> Text {
        Text(self.rope.clone())
    }

    fn byte_len(&self) -> usize {
        self.rope.byte_len()
    }

    fn replace(&mut self, range: ByteRange, text: &str) -> Result<String, EditError> {
        self.validate(&range)?;
        // `range` is validated (in bounds, on code-point boundaries), so slicing
        // the removed text cannot panic. Capture it before mutating so undo can
        // invert this edit (SPEC §2.4, §5).
        let removed = self.rope.byte_slice(range.clone()).to_string();
        self.rope.replace(range, text);
        Ok(removed)
    }

    fn position_of_byte(&self, byte_offset: usize) -> Position {
        self.text().position_of_byte(byte_offset)
    }

    fn byte_of_position(&self, pos: Position) -> Option<usize> {
        if pos.line >= self.rope.line_len() {
            // The one valid past-the-end case: an empty buffer or a buffer whose
            // final line has no terminator addresses line 0 / the last line at
            // col 0..=len. Everything else past the line count is invalid.
            if pos.line == self.rope.line_len() && pos.col == 0 {
                return Some(self.rope.byte_len());
            }
            return None;
        }
        let line_start = self.rope.byte_of_line(pos.line);
        let line_bytes = self.rope.line(pos.line).byte_len();
        if pos.col > line_bytes {
            return None;
        }
        Some(line_start + pos.col)
    }
}

#[cfg(test)]
#[path = "buffer_tests.rs"]
mod tests;
