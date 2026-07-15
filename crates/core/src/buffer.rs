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
    /// # Errors
    /// Returns [`EditError`] if the range is out of bounds or not on a UTF-8 code
    /// point boundary - never panics on bad input (SPEC §8).
    fn replace(&mut self, range: ByteRange, text: &str) -> Result<(), EditError>;

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
    /// `0` and `byte_len()` are always boundaries.
    ///
    /// crop's offset-based methods (`byte_slice`, `line_of_byte`) *panic* if the
    /// offset is not on a code point boundary - the exact input this guards
    /// against - so we never hand `offset` to one. Line-index methods
    /// (`byte_of_line`) always return boundary offsets, so we binary-search the
    /// line-start offsets to locate the line containing `offset`, materialize
    /// just that line (its endpoints are boundaries, so the slice is safe), and
    /// defer to `str::is_char_boundary`. Cost is O(log(lines)) lookups plus one
    /// line's length - bounded, never a full-file scan (SPEC §10.4).
    fn is_char_boundary(&self, offset: usize) -> bool {
        let len = self.rope.byte_len();
        if offset == 0 || offset == len {
            return true;
        }
        if offset > len {
            return false;
        }
        // len > 0 here, so there is at least one line. Find the largest line
        // index whose start offset is <= `offset` (upper-mid to guarantee
        // progress when `lo == mid`).
        let line_count = self.rope.line_len();
        let (mut lo, mut hi) = (0, line_count - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.rope.byte_of_line(mid) <= offset {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let line_start = self.rope.byte_of_line(lo);
        let line_end = if lo + 1 < line_count {
            self.rope.byte_of_line(lo + 1)
        } else {
            len
        };
        // Slice spans the line *including* its terminator, so an `offset` at a
        // `\r`/`\n` is classified correctly too.
        self.rope
            .byte_slice(line_start..line_end)
            .to_string()
            .is_char_boundary(offset - line_start)
    }
}

impl Buffer for RopeBuffer {
    fn text(&self) -> Text {
        Text(self.rope.clone())
    }

    fn byte_len(&self) -> usize {
        self.rope.byte_len()
    }

    fn replace(&mut self, range: ByteRange, text: &str) -> Result<(), EditError> {
        self.validate(&range)?;
        // crop has no atomic replace; delete-then-insert at the same offset is
        // equivalent and both endpoints are already validated.
        if !range.is_empty() {
            self.rope.delete(range.clone());
        }
        if !text.is_empty() {
            self.rope.insert(range.start, text);
        }
        Ok(())
    }

    fn position_of_byte(&self, byte_offset: usize) -> Position {
        let offset = byte_offset.min(self.rope.byte_len());
        let line = self.rope.line_of_byte(offset);
        let line_start = self.rope.byte_of_line(line);
        Position::new(line, offset - line_start)
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
mod tests {
    use super::*;

    // A grapheme cluster spanning several code points: the classic adversarial
    // case for byte<->position bugs (SPEC §4, §13). 25 bytes total.
    const FAMILY: &str = "👨‍👩‍👧"; // man+ZWJ+woman+ZWJ+girl

    #[test]
    fn empty_buffer_basics() {
        let b = RopeBuffer::new();
        assert_eq!(b.byte_len(), 0);
        let t = b.text();
        assert!(t.is_empty());
        assert_eq!(t.line_count(), 0);
        assert_eq!(t.to_string(), "");
    }

    #[test]
    fn insert_via_replace_empty_range() {
        let mut b = RopeBuffer::new();
        b.replace(0..0, "hello").unwrap();
        assert_eq!(b.text().to_string(), "hello");
        b.replace(5..5, " world").unwrap();
        assert_eq!(b.text().to_string(), "hello world");
    }

    #[test]
    fn delete_via_replace_empty_text() {
        let mut b = RopeBuffer::from("hello world");
        b.replace(5..11, "").unwrap();
        assert_eq!(b.text().to_string(), "hello");
    }

    #[test]
    fn replace_swaps_range_for_text() {
        let mut b = RopeBuffer::from("hello world");
        b.replace(6..11, "there").unwrap();
        assert_eq!(b.text().to_string(), "hello there");
    }

    #[test]
    fn line_count_ignores_trailing_newline() {
        // crop: a final line break is not a separate empty line (verified §3).
        assert_eq!(RopeBuffer::from("a\nb\n").text().line_count(), 2);
        assert_eq!(RopeBuffer::from("a\nb").text().line_count(), 2);
        assert_eq!(RopeBuffer::from("a").text().line_count(), 1);
    }

    #[test]
    fn line_accessor_excludes_terminator_and_bounds() {
        let t = RopeBuffer::from("first\nsecond\nthird").text();
        assert_eq!(t.line(0).as_deref(), Some("first"));
        assert_eq!(t.line(1).as_deref(), Some("second"));
        assert_eq!(t.line(2).as_deref(), Some("third"));
        assert_eq!(t.line(3), None);
    }

    #[test]
    fn line_len_matches_line_byte_length() {
        // Multibyte: "日本" is 6 bytes, no terminator counted.
        let t = RopeBuffer::from("first\n日本\nx").text();
        assert_eq!(t.line_len(0), Some(5)); // "first"
        assert_eq!(t.line_len(1), Some(6)); // "日本"
        assert_eq!(t.line_len(2), Some(1)); // "x"
        assert_eq!(t.line_len(3), None); // out of range
        // Agrees with materializing the line, but without the allocation.
        for i in 0..t.line_count() {
            assert_eq!(t.line_len(i), t.line(i).map(|l| l.len()));
        }
    }

    #[test]
    fn byte_of_line_reports_start_offsets() {
        let t = RopeBuffer::from("ab\ncde\nf").text(); // 3 lines, 8 bytes
        assert_eq!(t.byte_of_line(0), Some(0));
        assert_eq!(t.byte_of_line(1), Some(3)); // after "ab\n"
        assert_eq!(t.byte_of_line(2), Some(7)); // after "cde\n"
        // line_index == line_count addresses the buffer end (parallels
        // byte_of_position's past-the-end case); beyond that is None.
        assert_eq!(t.byte_of_line(3), Some(8));
        assert_eq!(t.byte_of_line(4), None);
    }

    #[test]
    fn line_of_byte_clamps_past_end() {
        let t = RopeBuffer::from("ab\ncd").text();
        assert_eq!(t.line_of_byte(0), 0);
        assert_eq!(t.line_of_byte(3), 1);
        assert_eq!(t.line_of_byte(999), 1); // clamped to buffer end
    }

    #[test]
    fn position_byte_round_trip_ascii() {
        let b = RopeBuffer::from("ab\ncde\nf");
        for offset in 0..=b.byte_len() {
            let pos = b.position_of_byte(offset);
            assert_eq!(b.byte_of_position(pos), Some(offset), "offset {offset}");
        }
    }

    #[test]
    fn position_byte_round_trip_multibyte() {
        // CJK (3 bytes each) + a ZWJ emoji family: byte offsets skip whole
        // clusters, so this catches any 1-char-per-byte assumption (§4).
        let text = format!("日本語\n{FAMILY}\nx");
        let b = RopeBuffer::from(text.as_str());
        // Walk only code-point boundaries; positions are byte-columns.
        let mut offset = 0;
        for ch in text.chars() {
            let pos = b.position_of_byte(offset);
            assert_eq!(b.byte_of_position(pos), Some(offset), "offset {offset}");
            offset += ch.len_utf8();
        }
        // And the very end.
        let end = b.byte_len();
        assert_eq!(b.byte_of_position(b.position_of_byte(end)), Some(end));
    }

    #[test]
    fn position_of_byte_columns() {
        let b = RopeBuffer::from("ab\ncde");
        assert_eq!(b.position_of_byte(0), Position::new(0, 0));
        assert_eq!(b.position_of_byte(1), Position::new(0, 1));
        assert_eq!(b.position_of_byte(3), Position::new(1, 0)); // start of line 1
        assert_eq!(b.position_of_byte(5), Position::new(1, 2));
    }

    #[test]
    fn byte_of_position_rejects_out_of_range() {
        let b = RopeBuffer::from("ab\ncd");
        assert_eq!(b.byte_of_position(Position::new(0, 99)), None); // col past line
        assert_eq!(b.byte_of_position(Position::new(9, 0)), None); // line past count
    }

    #[test]
    fn byte_of_position_empty_buffer_end() {
        // An empty buffer has 0 lines; position (0,0) still addresses its end.
        let b = RopeBuffer::new();
        assert_eq!(b.byte_of_position(Position::new(0, 0)), Some(0));
    }

    #[test]
    fn replace_rejects_out_of_bounds() {
        let mut b = RopeBuffer::from("abc");
        assert_eq!(
            b.replace(2..10, "x"),
            Err(EditError::OutOfBounds {
                start: 2,
                end: 10,
                len: 3
            })
        );
        // Buffer is unchanged after a rejected edit.
        assert_eq!(b.text().to_string(), "abc");
    }

    #[test]
    fn replace_rejects_inverted_range() {
        let mut b = RopeBuffer::from("abc");
        // Build the range from runtime values so clippy's static empty-range lint
        // (this is intentionally inverted) does not fire.
        let (start, end) = (3, 1);
        assert_eq!(
            b.replace(start..end, "x"),
            Err(EditError::Inverted { start: 3, end: 1 })
        );
    }

    #[test]
    fn replace_rejects_non_char_boundary() {
        // "日" is 3 bytes; offset 1 splits it.
        let mut b = RopeBuffer::from("日");
        assert_eq!(
            b.replace(1..3, "x"),
            Err(EditError::NotCharBoundary { offset: 1 })
        );
        assert_eq!(b.text().to_string(), "日");
    }

    #[test]
    fn replace_rejects_non_char_boundary_on_later_line() {
        // A multi-line buffer forces is_char_boundary's binary search to iterate
        // (single-line buffers short-circuit). "語" starts at byte 6, so offset 7
        // splits it - the split is on line 2, not the first line.
        let mut b = RopeBuffer::from("ab\ncd\n語");
        assert_eq!(
            b.replace(7..9, "x"),
            Err(EditError::NotCharBoundary { offset: 7 })
        );
        // A valid boundary on that same later line is accepted.
        assert!(b.replace(6..9, "z").is_ok());
        assert_eq!(b.text().to_string(), "ab\ncd\nz");
    }

    #[test]
    fn replace_rejects_non_char_boundary_on_first_line_of_many() {
        // A split on the FIRST line of a multi-line buffer forces the boundary
        // search to walk its high end down (the `hi = mid - 1` branch), which a
        // split on the last line does not exercise. "日" splits at offset 1.
        let mut b = RopeBuffer::from("日\nab\ncd\nef");
        assert_eq!(
            b.replace(1..3, "x"),
            Err(EditError::NotCharBoundary { offset: 1 })
        );
    }

    #[test]
    fn replace_past_end_offset_on_multiline_is_out_of_bounds() {
        // Exercises the `offset > len` short-circuit in is_char_boundary via a
        // range whose end runs past the buffer.
        let mut b = RopeBuffer::from("ab\ncd");
        assert!(matches!(
            b.replace(3..99, "x"),
            Err(EditError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn text_byte_len_matches_buffer() {
        let b = RopeBuffer::from("日本語"); // 9 bytes
        assert_eq!(b.text().byte_len(), 9);
        assert_eq!(b.text().byte_len(), b.byte_len());
    }

    #[test]
    fn edit_error_messages_render() {
        // thiserror Display paths (used by Notification::Error later, §8).
        assert!(
            EditError::OutOfBounds {
                start: 2,
                end: 10,
                len: 3
            }
            .to_string()
            .contains("out of bounds")
        );
        assert!(
            EditError::Inverted { start: 3, end: 1 }
                .to_string()
                .contains("past its end")
        );
        assert!(
            EditError::NotCharBoundary { offset: 1 }
                .to_string()
                .contains("code point boundary")
        );
    }

    #[test]
    fn text_clone_is_independent_of_further_edits() {
        // A snapshot's Text must reflect the buffer at capture time, not follow
        // later edits (SPEC §5: snapshots are immutable at a version).
        let mut b = RopeBuffer::from("hello");
        let snap = b.text();
        b.replace(0..0, "X").unwrap();
        assert_eq!(snap.to_string(), "hello");
        assert_eq!(b.text().to_string(), "Xhello");
    }
}
