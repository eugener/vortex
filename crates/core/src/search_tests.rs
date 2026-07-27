use super::*;
use crate::buffer::{Buffer, RopeBuffer};

fn text(s: &str) -> Text {
    RopeBuffer::from(s).text()
}

/// Matches over the whole buffer, the way `select-all-matches` asks for them.
fn all(t: &Text, pattern: &str) -> Vec<Range<usize>> {
    let regex = compile(pattern).expect("a valid pattern");
    matches_in(t, &regex, 0..t.line_count())
}

/// The next match forward from `from`, as a range.
fn next(t: &Text, pattern: &str, from: usize) -> Option<Range<usize>> {
    next_match(t, &compile(pattern).expect("a valid pattern"), from, false)
}

/// The previous match from `from`.
fn prev(t: &Text, pattern: &str, from: usize) -> Option<Range<usize>> {
    next_match(t, &compile(pattern).expect("a valid pattern"), from, true)
}

// --- compile / smart case -------------------------------------------------

#[test]
fn a_lowercase_pattern_ignores_case_and_a_capital_does_not() {
    // The rule the cross-file search already follows: typing lowercase says you do
    // not care, typing a capital says you did. The two searches share it because a
    // user does not hold two models of what their own typing means.
    let t = text("Needle\nneedle\nNEEDLE\n");
    assert_eq!(all(&t, "needle").len(), 3, "lowercase matches every case");
    let upper = all(&t, "Needle");
    assert_eq!(upper.len(), 1, "a capital is meant");
    assert_eq!(upper[0], 0..6, "the one that is spelled that way");
}

#[test]
fn a_pattern_that_is_not_a_regex_is_an_error_not_a_panic() {
    // A pattern is user input: it comes back as an error every caller reports.
    assert!(compile("unclosed(").is_err());
    assert!(compile("a{2,1}").is_err());
    assert!(compile(r"\p{NotAScript}").is_err());
}

// --- matches_in -----------------------------------------------------------

#[test]
fn matches_are_whole_buffer_ranges_in_ascending_order() {
    let t = text("one two\nthree two\n");
    assert_eq!(all(&t, "two"), vec![4..7, 14..17]);
}

#[test]
fn several_matches_on_one_line_all_count() {
    // Unlike the cross-file walk, which reports one hit per line because a row is a
    // place to jump to: here every match is a selection, so every one is a match.
    let t = text("aa aa aa\n");
    assert_eq!(all(&t, "aa"), vec![0..2, 3..5, 6..8]);
}

#[test]
fn the_pattern_is_a_regex_anchored_to_the_line() {
    // `^` and `$` mean the line, not the buffer - what a user typing `^fn ` means.
    let t = text("fn alpha() {}\n    fn beta() {}\nlet gamma = 1;\n");
    assert_eq!(all(&t, r"^fn \w+").len(), 1, "only the unindented one");
    assert_eq!(all(&t, r"fn \w+").len(), 2);
    assert_eq!(all(&t, r"\{\}$").len(), 2, "both lines end that way");
}

#[test]
fn a_match_never_spans_a_line_break() {
    // The line is the unit, so a pattern reaching across a terminator finds nothing
    // rather than silently matching the newline the buffer normalised to LF.
    let t = text("alpha\nbeta\n");
    assert!(all(&t, r"alpha\nbeta").is_empty());
    assert!(all(&t, "alpha.beta").is_empty());
}

#[test]
fn empty_matches_are_not_places() {
    // `x*` matches the empty string everywhere. Selecting all of them would put a
    // cursor in every gap and replacing them would splice text into each one.
    let t = text("abc\n");
    assert!(all(&t, "x*").is_empty());
    assert!(all(&t, "").is_empty());
    // A pattern that can match empty still reports the non-empty matches it makes.
    assert_eq!(all(&t, "b*"), vec![1..2]);
}

#[test]
fn only_the_requested_lines_are_searched() {
    // The range is the caller's cost control: a live highlight pays for the
    // viewport, not the file (SPEC §10.4).
    let t = text("hit\nhit\nhit\nhit\n");
    let regex = compile("hit").expect("a valid pattern");
    assert_eq!(matches_in(&t, &regex, 1..3), vec![4..7, 8..11]);
    assert!(matches_in(&t, &regex, 2..2).is_empty(), "an empty range");
}

#[test]
fn line_indices_past_the_buffer_are_skipped_not_refused() {
    // A frontend's viewport can outlive the snapshot it was computed against; it
    // gets the lines that exist rather than nothing at all.
    let t = text("hit\n");
    let regex = compile("hit").expect("a valid pattern");
    assert_eq!(matches_in(&t, &regex, 0..500), vec![0..3]);
    assert!(matches_in(&t, &regex, 90..99).is_empty());
}

#[test]
fn an_empty_buffer_matches_nothing() {
    let t = text("");
    assert!(all(&t, "anything").is_empty());
    assert_eq!(next(&t, "anything", 0), None);
    assert_eq!(prev(&t, "anything", 0), None);
}

#[test]
fn multibyte_text_yields_character_boundary_ranges() {
    // Every offset handed out must be a boundary a later slice cannot panic on.
    let t = text("le café est ouvert\ncafé\n");
    let found = all(&t, "café");
    assert_eq!(found, vec![3..8, 20..25]);
    for range in found {
        assert_eq!(t.slice(range), "café");
    }
}

// --- next_match -----------------------------------------------------------

#[test]
fn the_next_match_is_found_at_or_after_the_offset() {
    let t = text("two and two and two\n");
    assert_eq!(next(&t, "two", 0), Some(0..3));
    assert_eq!(next(&t, "two", 1), Some(8..11));
    assert_eq!(next(&t, "two", 8), Some(8..11), "at the offset counts");
}

#[test]
fn the_forward_search_crosses_lines_and_wraps_at_the_end() {
    let t = text("alpha\nbeta\ngamma target\n");
    assert_eq!(next(&t, "target", 0), Some(17..23), "found on a later line");
    // Past the only match: it wraps back to the start rather than giving up.
    assert_eq!(next(&t, "alpha", 10), Some(0..5));
}

#[test]
fn a_pattern_matching_only_where_the_caret_sits_still_wraps_back_to_it() {
    // With the wrap, a pattern that matches anywhere always has a next match - even
    // when the caret is already on the only one, which is what makes repeating the
    // key harmless rather than a dead end.
    let t = text("lonely\n");
    assert_eq!(next(&t, "lonely", 3), Some(0..6));
}

#[test]
fn the_backward_search_takes_the_match_strictly_before_the_offset() {
    let t = text("two and two and two\n");
    assert_eq!(prev(&t, "two", 19), Some(16..19));
    assert_eq!(prev(&t, "two", 16), Some(8..11), "strictly before");
    assert_eq!(prev(&t, "two", 8), Some(0..3));
}

#[test]
fn the_backward_search_wraps_to_the_end_of_the_buffer() {
    let t = text("first\nmiddle\nlast\n");
    // Nothing before offset 0, so it wraps around to the last match in the file.
    assert_eq!(
        prev(&t, "first", 0),
        Some(0..5),
        "the only match, wrapped to"
    );
    assert_eq!(prev(&t, r"^\w+", 0), Some(13..17), "last line's match");
}

#[test]
fn walking_forward_then_back_returns_to_the_previous_match() {
    // The asymmetry that makes repeat-search work: forward is "at or after" so the
    // caller advances past a match, and backward is "strictly before" so searching
    // back from a match's own start reaches the one before it, not itself.
    let t = text("a x a x a\n");
    let second = next(&t, "a", 1).expect("a second match");
    assert_eq!(second, 4..5);
    assert_eq!(prev(&t, "a", second.start), Some(0..1));
}

#[test]
fn an_offset_past_the_buffer_is_clamped_rather_than_panicking() {
    let t = text("needle\n");
    assert_eq!(
        next(&t, "needle", 9_999),
        Some(0..6),
        "clamped, then wrapped"
    );
    assert_eq!(prev(&t, "needle", 9_999), Some(0..6));
}

#[test]
fn a_caret_on_a_line_terminator_searches_from_the_lines_end() {
    // A column can sit past the content when the caret is on the terminator; the
    // limit clamps to the content rather than slicing out of range.
    let t = text("ab\ncd\n");
    assert_eq!(next(&t, "cd", 2), Some(3..5));
}

#[test]
fn no_match_anywhere_is_none_in_both_directions() {
    let t = text("alpha\nbeta\n");
    assert_eq!(next(&t, "zeta", 0), None);
    assert_eq!(prev(&t, "zeta", 5), None);
}

// --- replacements_in ------------------------------------------------------

/// The whole buffer's replacements for `pattern` -> `template`.
fn replace_all(t: &Text, pattern: &str, template: &str) -> Vec<(Range<usize>, String)> {
    let regex = compile(pattern).expect("a valid pattern");
    replacements_in(t, &regex, 0..t.line_count(), template)
}

#[test]
fn every_match_is_paired_with_its_replacement() {
    let t = text("cat dog cat\n");
    assert_eq!(
        replace_all(&t, "cat", "bird"),
        vec![(0..3, "bird".into()), (8..11, "bird".into())]
    );
}

#[test]
fn capture_references_expand_against_their_own_match() {
    // The reason a regex replace is worth having: putting back what was captured.
    let t = text("alpha beta\ngamma delta\n");
    assert_eq!(
        replace_all(&t, r"(\w+) (\w+)", "$2 $1"),
        vec![(0..10, "beta alpha".into()), (11..22, "delta gamma".into()),]
    );
}

#[test]
fn a_named_group_expands_and_a_missing_one_expands_to_nothing() {
    let t = text("key = value\n");
    assert_eq!(
        replace_all(&t, r"(?<k>\w+) = (?<v>\w+)", "$v = $k"),
        vec![(0..11, "value = key".into())]
    );
    // `regex`'s own rule for a group that did not participate.
    assert_eq!(
        replace_all(&t, r"(\w+)(!)?", "[$1|$2]"),
        vec![(0..3, "[key|]".into()), (6..11, "[value|]".into())]
    );
}

#[test]
fn a_literal_dollar_is_written_twice() {
    let t = text("price\n");
    assert_eq!(replace_all(&t, "price", "$$5"), vec![(0..5, "$5".into())]);
}

#[test]
fn captures_come_from_the_match_in_context_not_from_a_second_pass() {
    // `a|ab` matching `ab` would re-match as just `a` if the replacement re-ran the
    // pattern over the matched text, losing the alternation's context. One pass is
    // both cheaper and the only correct one.
    let t = text("ab\n");
    assert_eq!(
        replace_all(&t, "(ab)|(a)", "<$1>"),
        vec![(0..2, "<ab>".into())]
    );
}

#[test]
fn an_empty_replacement_deletes_the_match() {
    let t = text("keep DROP keep\n");
    assert_eq!(replace_all(&t, "DROP ", ""), vec![(5..10, String::new())]);
}

#[test]
fn empty_matches_produce_no_replacements() {
    // The same rule as `matches_in`: splicing the template into every gap is never
    // what "replace all" meant.
    let t = text("abc\n");
    assert!(replace_all(&t, "x*", "!").is_empty());
}

#[test]
fn replacements_are_ascending_so_the_caller_reverses_once() {
    let t = text("x\nx\nx\n");
    let found = replace_all(&t, "x", "y");
    assert!(
        found.windows(2).all(|w| w[0].0.start < w[1].0.start),
        "{found:?}"
    );
}

#[test]
fn a_single_lines_replacements_are_reachable_on_their_own() {
    // The "replace this one" path asks for the primary's line alone rather than the
    // whole buffer.
    let t = text("hit\nhit\nhit\n");
    let regex = compile("hit").expect("a valid pattern");
    assert_eq!(
        replacements_in(&t, &regex, 1..2, "got"),
        vec![(4..7, "got".into())]
    );
}

#[test]
fn replacement_line_indices_past_the_buffer_are_skipped() {
    // Same forgiveness as `matches_in`: a caller holding a range computed against a
    // slightly older buffer gets the lines that exist rather than nothing.
    let t = text("hit\n");
    let regex = compile("hit").expect("a valid pattern");
    assert_eq!(
        replacements_in(&t, &regex, 0..500, "got"),
        vec![(0..3, "got".into())]
    );
    assert!(replacements_in(&t, &regex, 90..99, "got").is_empty());
}
