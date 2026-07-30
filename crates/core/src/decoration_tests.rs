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

fn highlight(range: ByteRange, kind: HighlightKind) -> Decoration {
    Decoration::Highlight { range, kind }
}

fn syntax_set(decorations: Vec<Decoration>) -> DecorationSet {
    let mut s = DecorationSet::new();
    s.replace(DecorationSource::Syntax, decorations);
    s
}

#[test]
fn highlights_in_clips_spans_to_the_queried_range() {
    // Same per-line resolution as underlines: the frontend paints one line and
    // needs only the piece of the span on it.
    let s = syntax_set(vec![highlight(2..12, HighlightKind::Function)]);
    assert_eq!(
        s.highlights_in(5..9).collect::<Vec<_>>(),
        vec![(5..9, HighlightKind::Function)]
    );
}

#[test]
fn highlights_in_returns_only_spans_on_the_line_over_many_sorted_spans() {
    // Exercises the binary-search path (#2): with many spans, a per-line query
    // returns only the overlapping ones - including a multi-line span that started
    // before the query window - and nothing past it. Input order is arbitrary;
    // `replace` sorts by start.
    let s = syntax_set(vec![
        highlight(30..33, HighlightKind::Type),
        highlight(0..3, HighlightKind::Keyword),
        highlight(5..20, HighlightKind::Comment), // multi-line, spans the mid window
        highlight(25..28, HighlightKind::Function),
    ]);
    // Mid window: only the long comment overlaps, clipped to the window.
    assert_eq!(
        s.highlights_in(10..15).collect::<Vec<_>>(),
        vec![(10..15, HighlightKind::Comment)]
    );
    // Window straddling two spans, each clipped.
    assert_eq!(
        s.highlights_in(2..6).collect::<Vec<_>>(),
        vec![
            (2..3, HighlightKind::Keyword),
            (5..6, HighlightKind::Comment)
        ]
    );
    // Past every span.
    assert_eq!(s.highlights_in(40..50).count(), 0);
    // Before every span.
    assert_eq!(
        s.highlights_in(0..1).collect::<Vec<_>>(),
        vec![(0..1, HighlightKind::Keyword)]
    );
}

#[test]
fn highlights_in_excludes_spans_that_only_touch_the_range_edge() {
    let s = syntax_set(vec![highlight(0..5, HighlightKind::Keyword)]);
    assert_eq!(s.highlights_in(5..9).count(), 0);
    assert_eq!(s.highlights_in(4..9).count(), 1);
}

#[test]
fn highlights_in_ignores_underlines_and_gutter_marks() {
    // The resolver is kind-specific: a diagnostic underline sharing the buffer
    // must not surface as a highlight (and vice versa via `underlines_in`).
    let mut s = syntax_set(vec![highlight(0..4, HighlightKind::Type)]);
    s.replace(
        DecorationSource::Lsp,
        vec![underline(0..4, Severity::Error)],
    );
    assert_eq!(s.highlights_in(0..9).count(), 1);
    assert_eq!(s.underlines_in(0..9).count(), 1);
}

#[test]
fn a_highlight_and_a_diagnostic_coexist_on_one_cell() {
    // The whole reason `Highlight` is a distinct variant (SPEC §5): a cell can
    // carry a syntax color and an independent squiggle at once, from different
    // producer buckets, and neither resolver sees the other's spans.
    let mut s = syntax_set(vec![highlight(0..4, HighlightKind::Variable)]);
    s.replace(
        DecorationSource::Lsp,
        vec![underline(0..4, Severity::Warning)],
    );
    assert_eq!(
        s.highlights_in(0..4).collect::<Vec<_>>(),
        vec![(0..4, HighlightKind::Variable)]
    );
    assert_eq!(
        s.underlines_in(0..4).collect::<Vec<_>>(),
        vec![(0..4, Severity::Warning)]
    );
}

#[test]
fn replacing_syntax_leaves_the_lsp_bucket_untouched() {
    // A reparse republishes only the Syntax bucket; diagnostics survive it. This
    // is the producer independence M4 relies on to land without touching M2.
    let mut s = syntax_set(vec![highlight(0..4, HighlightKind::Type)]);
    s.replace(
        DecorationSource::Lsp,
        vec![underline(6..9, Severity::Error)],
    );
    s.replace(
        DecorationSource::Syntax,
        vec![highlight(0..2, HighlightKind::Keyword)],
    );
    assert_eq!(
        s.highlights_in(0..9).collect::<Vec<_>>(),
        vec![(0..2, HighlightKind::Keyword)]
    );
    assert_eq!(s.underlines_in(0..9).count(), 1);
}

#[test]
fn a_highlight_rides_edits_with_the_shift_dont_grow_bias() {
    // Highlights transform through the same channel as underlines and with the
    // same bias: typing at a token's start shifts its color along (the new text
    // stays uncolored until the next reparse), never swallowed into the span.
    let mut s = syntax_set(vec![highlight(4..7, HighlightKind::Function)]);
    s.transform_through(&[edit(4, 4, 2)]); // type 2 bytes at the span start
    assert_eq!(
        s.highlights_in(0..99).collect::<Vec<_>>(),
        vec![(6..9, HighlightKind::Function)]
    );
    s.transform_through(&[edit(9, 9, 2)]); // type 2 bytes at the span end
    assert_eq!(
        s.highlights_in(0..99).collect::<Vec<_>>(),
        vec![(6..9, HighlightKind::Function)]
    );
}

fn scope_set(ranges: &[(usize, usize)]) -> DecorationSet {
    let mut s = DecorationSet::new();
    s.replace(
        DecorationSource::Scope,
        ranges
            .iter()
            .map(|&(start, end)| Decoration::Scope { range: start..end })
            .collect(),
    );
    s
}

/// The one scope `set` reports at `offset`, or a failure naming what it found -
/// spelled out rather than compared against a one-element `Vec`, which reads as a
/// range someone meant to expand.
fn only_scope_at(set: &DecorationSet, offset: usize) -> ByteRange {
    let found: Vec<_> = set.scopes_at(offset).collect();
    assert_eq!(found.len(), 1, "expected exactly one scope: {found:?}");
    found[0].clone()
}

#[test]
fn scopes_at_returns_the_enclosing_chain_outermost_first() {
    // The header's own order: a scope containing another starts no later, and the
    // bucket is sorted by start, so iteration reads module -> impl -> fn.
    let s = scope_set(&[(10, 90), (0, 100), (20, 40)]);
    assert_eq!(
        s.scopes_at(30).collect::<Vec<_>>(),
        vec![0..100, 10..90, 20..40]
    );
}

#[test]
fn a_scope_starting_at_the_queried_offset_does_not_enclose_it() {
    // The frontend asks with the first byte of the top visible row: a scope
    // starting there has its own first line on screen already, and pinning it
    // would print that line twice.
    let s = scope_set(&[(10, 90)]);
    assert_eq!(s.scopes_at(10).count(), 0);
    assert_eq!(only_scope_at(&s, 11), 10..90);
}

#[test]
fn a_scope_ending_at_the_queried_offset_does_not_enclose_it() {
    // Its last byte is on an earlier row, so it covers no cell of this one.
    let s = scope_set(&[(10, 90)]);
    assert_eq!(s.scopes_at(90).count(), 0);
    assert_eq!(only_scope_at(&s, 89), 10..90);
}

#[test]
fn scopes_at_ignores_every_other_producer() {
    // Only the Scope bucket can hold one, which is what lets the lookup skip the
    // file's thousands of highlights entirely.
    let mut s = scope_set(&[(0, 100)]);
    s.replace(
        DecorationSource::Syntax,
        vec![highlight(10..20, HighlightKind::Function)],
    );
    s.replace(
        DecorationSource::Lsp,
        vec![underline(10..20, Severity::Error)],
    );
    assert_eq!(only_scope_at(&s, 15), 0..100);
}

#[test]
fn scopes_at_reads_the_kind_not_just_the_bucket() {
    // The bucket says who published, the variant says what it is - and the lookup
    // asks the variant. A producer that put something else in this bucket gets it
    // ignored rather than misread as a scope.
    let mut s = scope_set(&[(0, 100)]);
    s.replace(
        DecorationSource::Scope,
        vec![
            Decoration::Scope { range: 0..100 },
            underline(10..20, Severity::Error),
        ],
    );
    assert_eq!(only_scope_at(&s, 15), 0..100);
}

#[test]
fn nested_scopes_do_not_disturb_the_highlight_search() {
    // Why scopes get their own bucket: their ends are not monotonic, and
    // `highlights_in` binary-searches a bucket assuming they are. In their own
    // bucket the search finds nothing to misplace, whatever it lands on.
    let mut s = scope_set(&[(0, 100), (10, 90), (20, 40)]);
    s.replace(
        DecorationSource::Syntax,
        vec![
            highlight(0..3, HighlightKind::Keyword),
            highlight(30..33, HighlightKind::Type),
        ],
    );
    assert_eq!(
        s.highlights_in(30..34).collect::<Vec<_>>(),
        vec![(30..33, HighlightKind::Type)]
    );
    assert_eq!(s.underlines_in(0..100).count(), 0);
    assert_eq!(s.gutter_mark(&RopeBuffer::from("x").text(), 0), None);
}

#[test]
fn a_scope_rides_edits_with_the_shift_dont_grow_bias() {
    // Typing inside a function moves its end along, so the header keeps naming it
    // between reparses; typing past its closing brace leaves the scope alone
    // rather than stretching it over what follows.
    let mut s = scope_set(&[(10, 90)]);
    s.transform_through(&[edit(50, 50, 5)]); // 5 bytes inside the body
    assert_eq!(only_scope_at(&s, 30), 10..95);
    s.transform_through(&[edit(95, 95, 5)]); // 5 bytes just past the end
    assert_eq!(only_scope_at(&s, 30), 10..95);
    s.transform_through(&[edit(10, 10, 2)]); // 2 bytes just before the start
    assert_eq!(only_scope_at(&s, 30), 12..97);
}
