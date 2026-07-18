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
fn replace_returns_the_removed_text() {
    // The removed text is the inverse info undo needs (SPEC §2.4, §5): a replace
    // hands it back, sliced from the pre-edit content of the range.
    let mut b = RopeBuffer::from("hello world");
    assert_eq!(b.replace(6..11, "there").unwrap(), "world"); // replaced span
    assert_eq!(b.replace(0..0, "X").unwrap(), ""); // pure insert removes nothing
    assert_eq!(b.replace(0..1, "").unwrap(), "X"); // pure delete returns what it removed
}

#[test]
fn replace_returns_removed_text_across_multibyte_graphemes() {
    // Removed text is sliced on validated code-point boundaries, so a multibyte
    // grapheme round-trips intact (SPEC §4).
    let mut b = RopeBuffer::from("a語b");
    // "語" is 3 bytes at offset 1..4.
    assert_eq!(b.replace(1..4, "").unwrap(), "語");
    assert_eq!(b.text().to_string(), "ab");
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
    // A range whose end runs past the buffer is rejected by the bounds check before
    // the boundary check ever runs.
    let mut b = RopeBuffer::from("ab\ncd");
    assert!(matches!(
        b.replace(3..99, "x"),
        Err(EditError::OutOfBounds { .. })
    ));
}

#[test]
fn is_char_boundary_past_end_is_false() {
    // The defensive `offset > len` guard: callers (replace) pre-validate bounds, so
    // this never fires in practice, but the helper must not hand an out-of-range
    // offset to crop (which would panic). Exercised directly since no public path
    // reaches it.
    let b = RopeBuffer::from("ab\ncd"); // len 5
    assert!(!b.is_char_boundary(6));
    assert!(!b.is_char_boundary(usize::MAX));
    // Sanity: in-range boundaries and the two endpoints still report true.
    assert!(b.is_char_boundary(0));
    assert!(b.is_char_boundary(5));
    assert!(b.is_char_boundary(2));
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

#[test]
fn text_slice_returns_the_byte_range() {
    // The copy path reads a selection's bytes via `slice` (SPEC §11).
    let text = RopeBuffer::from("hello world").text();
    assert_eq!(text.slice(0..5), "hello");
    assert_eq!(text.slice(6..11), "world");
    assert_eq!(text.slice(3..3), ""); // empty range
}

#[test]
fn text_slice_clamps_past_the_end_and_defends_bad_ranges() {
    // Defensive (SPEC §8): an end past the buffer clamps rather than panicking; an
    // inverted range (start > clamped end) yields "".
    let text = RopeBuffer::from("abc").text();
    assert_eq!(text.slice(1..99), "bc"); // end clamped to len
    // Start past the (clamped) end: built from variables so it is not a literal
    // reversed range (which clippy rejects at compile time). Yields "", no panic.
    let (start, end) = (5, 2);
    assert_eq!(text.slice(start..end), "");
}

#[test]
fn text_slice_off_a_char_boundary_yields_empty_not_a_panic() {
    // "é" is two UTF-8 bytes; slicing at offset 1 splits the code point. crop's
    // byte_slice would panic, so `slice` guards and returns "" (SPEC §8).
    let text = RopeBuffer::from("é").text();
    assert_eq!(text.byte_len(), 2);
    assert_eq!(text.slice(0..1), ""); // mid-code-point end
    assert_eq!(text.slice(0..2), "é"); // full code point is fine
}
