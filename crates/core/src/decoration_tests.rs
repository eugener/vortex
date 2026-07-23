use super::*;
use crate::buffer::{Buffer, RopeBuffer};

fn underline(range: ByteRange, severity: Severity) -> Decoration {
    Decoration::Underline { range, severity }
}

fn set(decorations: Vec<Decoration>) -> DecorationSet {
    let mut s = DecorationSet::new();
    s.replace(DecorationSource::Lsp, decorations);
    s
}

/// One edit replacing `start..old_end` with `insert_len` bytes.
fn edit(start: usize, old_end: usize, insert_len: usize) -> Edit {
    Edit {
        start,
        old_end,
        insert_len,
    }
}

#[test]
fn empty_set_reports_empty() {
    assert!(DecorationSet::new().is_empty());
    assert!(set(vec![]).is_empty());
    assert!(!set(vec![underline(0..3, Severity::Error)]).is_empty());
}

#[test]
fn replacing_a_source_with_nothing_clears_only_that_source() {
    // The producer contract: publishDiagnostics with an empty list means "this
    // file is clean now", which must actually remove the previous squiggles.
    let mut s = set(vec![underline(0..3, Severity::Error)]);
    assert!(!s.is_empty());
    s.replace(DecorationSource::Lsp, vec![]);
    assert!(s.is_empty());
}

#[test]
fn underlines_in_clips_spans_to_the_queried_range() {
    // The frontend queries one painted line at a time and needs the piece of the
    // span that lands on *that* line, not the whole span.
    let s = set(vec![underline(2..12, Severity::Warning)]);
    let found: Vec<_> = s.underlines_in(5..9).collect();
    assert_eq!(found, vec![(5..9, Severity::Warning)]);
}

#[test]
fn underlines_in_excludes_spans_that_only_touch_the_range_edge() {
    // A span ending exactly where the query starts covers no cell in it; painting
    // it would put a squiggle under the wrong character.
    let s = set(vec![underline(0..5, Severity::Error)]);
    assert_eq!(s.underlines_in(5..9).count(), 0);
    assert_eq!(s.underlines_in(4..9).count(), 1);
}

#[test]
fn underlines_in_ignores_gutter_marks() {
    let s = set(vec![Decoration::GutterMark {
        offset: 3,
        kind: GutterKind::Diagnostic(Severity::Error),
    }]);
    assert_eq!(s.underlines_in(0..99).count(), 0);
}

#[test]
fn gutter_mark_resolves_an_offset_to_its_line() {
    let text = RopeBuffer::from("ab\ncd\nef").text();
    let s = set(vec![
        Decoration::GutterMark {
            offset: 4, // inside "cd", line 1
            kind: GutterKind::Diagnostic(Severity::Warning),
        },
        // A real diagnostic contributes an underline *and* a gutter mark, so the
        // line query must skip the underline rather than trip over it.
        underline(3..5, Severity::Warning),
    ]);
    assert_eq!(s.gutter_mark(&text, 0), None);
    assert_eq!(
        s.gutter_mark(&text, 1),
        Some(GutterKind::Diagnostic(Severity::Warning))
    );
    assert_eq!(s.gutter_mark(&text, 2), None);
}

#[test]
fn gutter_mark_keeps_the_most_severe_when_a_line_has_several() {
    // The gutter has one cell per line; an error and a hint on the same line must
    // show the error.
    let text = RopeBuffer::from("ab\ncd").text();
    let s = set(vec![
        Decoration::GutterMark {
            offset: 3,
            kind: GutterKind::Diagnostic(Severity::Hint),
        },
        Decoration::GutterMark {
            offset: 4,
            kind: GutterKind::Diagnostic(Severity::Error),
        },
    ]);
    assert_eq!(
        s.gutter_mark(&text, 1),
        Some(GutterKind::Diagnostic(Severity::Error))
    );
}

#[test]
fn severity_orders_least_to_most_severe() {
    // `gutter_mark` picks with `max()`, so this ordering is load-bearing.
    assert!(Severity::Hint < Severity::Information);
    assert!(Severity::Information < Severity::Warning);
    assert!(Severity::Warning < Severity::Error);
}

#[test]
fn an_edit_before_a_span_shifts_it_by_the_length_change() {
    let mut s = set(vec![underline(10..15, Severity::Error)]);
    s.transform_through(&[edit(0, 0, 3)]); // insert 3 bytes at the start
    assert_eq!(
        s.underlines_in(0..99).collect::<Vec<_>>(),
        vec![(13..18, Severity::Error)]
    );
}

#[test]
fn an_edit_after_a_span_leaves_it_alone() {
    let mut s = set(vec![underline(2..5, Severity::Error)]);
    s.transform_through(&[edit(20, 20, 7)]);
    assert_eq!(
        s.underlines_in(0..99).collect::<Vec<_>>(),
        vec![(2..5, Severity::Error)]
    );
}

#[test]
fn typing_at_either_edge_shifts_a_span_instead_of_growing_it() {
    // The documented bias choice (start After, end Before), and the reason it is
    // the opposite of a selection's: an underline must keep covering the token it
    // flagged, not swallow whatever the user types next to it.
    let mut s = set(vec![underline(4..7, Severity::Error)]);
    s.transform_through(&[edit(4, 4, 2)]); // type 2 bytes at the span start
    assert_eq!(
        s.underlines_in(0..99).collect::<Vec<_>>(),
        vec![(6..9, Severity::Error)],
        "insertion at the start should push the span right, not extend it left"
    );

    let mut s = set(vec![underline(4..7, Severity::Error)]);
    s.transform_through(&[edit(7, 7, 2)]); // type 2 bytes at the span end
    assert_eq!(
        s.underlines_in(0..99).collect::<Vec<_>>(),
        vec![(4..7, Severity::Error)],
        "insertion at the end should leave the span, not extend it right"
    );
}

#[test]
fn deleting_the_flagged_text_collapses_the_span_to_nothing() {
    // Delete the erroneous token and the squiggle disappears rather than hanging
    // over whatever slid into its place before the server republishes.
    let mut s = set(vec![underline(4..7, Severity::Error)]);
    s.transform_through(&[edit(4, 7, 0)]);
    assert_eq!(
        s.underlines_in(0..99).count(),
        0,
        "a collapsed span must paint nothing"
    );
}

#[test]
fn a_span_never_inverts_under_an_edit() {
    // The end is Before-biased and the start After-biased, so a deletion covering
    // the span drives them toward each other; the invariant is that end never
    // lands left of start (an inverted range would panic a later slice).
    for (start, old_end, insert_len) in [(0, 20, 0), (5, 6, 0), (4, 7, 1), (0, 100, 3)] {
        let mut s = set(vec![underline(4..7, Severity::Error)]);
        s.transform_through(&[edit(start, old_end, insert_len)]);
        for (range, _) in s.underlines_in(0..999) {
            assert!(
                range.start <= range.end,
                "span inverted under edit {start}..{old_end} +{insert_len}: {range:?}"
            );
        }
    }
}

#[test]
fn a_gutter_mark_rides_an_inserted_line_downward() {
    // Stored as an offset, not a line index, exactly so this works without the
    // producer republishing.
    let text = RopeBuffer::from("ab\ncd\nef").text();
    let mut s = set(vec![Decoration::GutterMark {
        offset: 4, // line 1
        kind: GutterKind::Diagnostic(Severity::Error),
    }]);
    assert_eq!(
        s.gutter_mark(&text, 1),
        Some(GutterKind::Diagnostic(Severity::Error))
    );

    // Insert a whole line at the top; the mark must follow its text to line 2.
    let mut buffer = RopeBuffer::from("ab\ncd\nef");
    buffer.replace(0..0, "new\n").unwrap();
    s.transform_through(&[edit(0, 0, 4)]);
    let text = buffer.text();
    assert_eq!(s.gutter_mark(&text, 1), None);
    assert_eq!(
        s.gutter_mark(&text, 2),
        Some(GutterKind::Diagnostic(Severity::Error))
    );
}

#[test]
fn transform_composes_a_multi_cursor_batch_of_edits() {
    // One keystroke over N cursors is one batch of disjoint ascending edits; a
    // span after all of them shifts by their combined effect.
    let mut s = set(vec![underline(20..24, Severity::Error)]);
    s.transform_through(&[edit(0, 0, 1), edit(5, 5, 1), edit(9, 9, 1)]);
    assert_eq!(
        s.underlines_in(0..99).collect::<Vec<_>>(),
        vec![(23..27, Severity::Error)]
    );
}
