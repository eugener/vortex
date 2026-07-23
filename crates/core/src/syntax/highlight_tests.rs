use tree_sitter_highlight::{
    Error, Highlight, HighlightConfiguration, HighlightEvent, Highlighter,
};

use super::{kind_from_index, names, spans_from_events};
use crate::decoration::HighlightKind;
use crate::syntax::HighlightSpan;

/// The Rust grammar, configured with the producer's recognized names - the same
/// configuration the engine builds, so these tests exercise the real mapping.
fn rust_config() -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        tree_sitter_rust::INJECTIONS_QUERY,
        "",
    )
    .expect("the bundled Rust highlight query compiles against its own grammar");
    config.configure(&names());
    config
}

/// Highlight `source` as Rust and fold to spans - the whole pure pipeline.
fn highlights(source: &str) -> Vec<HighlightSpan> {
    let config = rust_config();
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&config, source.as_bytes(), None, |_| None)
        .expect("highlighting valid Rust does not error");
    spans_from_events(events).expect("folding a real event stream does not error")
}

/// Each highlighted span paired with the source text it covers, for robust
/// assertions that do not hard-code byte offsets (which shift with grammar
/// versions).
fn covered(source: &str) -> Vec<(String, HighlightKind)> {
    highlights(source)
        .into_iter()
        .map(|s| (source[s.range].to_string(), s.kind))
        .collect()
}

/// The index of a recognized name, for building hand-made event streams.
fn name_index(name: &str) -> usize {
    names()
        .iter()
        .position(|n| *n == name)
        .expect("recognized name is present")
}

#[test]
fn every_recognized_name_maps_to_a_kind_and_out_of_range_does_not() {
    // The `Highlight(i)` the highlighter emits is an index into `names()`, so the
    // table must map every one of those indices - and defend the out-of-range case
    // rather than panic (SPEC §8).
    for i in 0..names().len() {
        assert!(kind_from_index(i).is_some(), "index {i} has no kind");
    }
    assert_eq!(kind_from_index(names().len()), None);
}

#[test]
fn keyword_and_function_name_are_highlighted() {
    let spans = covered("fn main() {}");
    assert!(
        spans.contains(&("fn".to_string(), HighlightKind::Keyword)),
        "expected `fn` as a keyword, got {spans:?}"
    );
    assert!(
        spans.contains(&("main".to_string(), HighlightKind::Function)),
        "expected `main` as a function, got {spans:?}"
    );
}

#[test]
fn a_string_literal_is_highlighted() {
    let spans = covered(r#"fn f() { let s = "hi"; }"#);
    assert!(
        spans.contains(&("\"hi\"".to_string(), HighlightKind::String)),
        "expected the string literal highlighted, got {spans:?}"
    );
}

#[test]
fn a_line_comment_is_highlighted() {
    let spans = covered("// note\nfn f() {}");
    assert!(
        spans.contains(&("// note".to_string(), HighlightKind::Comment)),
        "expected the comment highlighted, got {spans:?}"
    );
}

#[test]
fn a_builtin_type_stays_distinct_from_a_plain_type() {
    // `type.builtin` has its own recognized-name row, so `i32` collapses to
    // TypeBuiltin rather than the plain Type a user struct would get.
    let spans = covered("fn f(n: i32) {}");
    assert!(
        spans.contains(&("i32".to_string(), HighlightKind::TypeBuiltin)),
        "expected `i32` as a builtin type, got {spans:?}"
    );
}

#[test]
fn empty_source_yields_no_spans() {
    assert!(highlights("").is_empty());
}

#[test]
fn the_innermost_highlight_colors_a_source_run() {
    // A nested stream: Type wraps everything, Function wraps the middle. The middle
    // run takes Function (top of stack), the outer runs take Type.
    let ty = name_index("type");
    let func = name_index("function");
    let events = vec![
        Ok(HighlightEvent::HighlightStart(Highlight(ty))),
        Ok(HighlightEvent::Source { start: 0, end: 4 }),
        Ok(HighlightEvent::HighlightStart(Highlight(func))),
        Ok(HighlightEvent::Source { start: 4, end: 8 }),
        Ok(HighlightEvent::HighlightEnd),
        Ok(HighlightEvent::Source { start: 8, end: 10 }),
        Ok(HighlightEvent::HighlightEnd),
    ];
    let spans = spans_from_events(events.into_iter()).expect("no error");
    assert_eq!(
        spans,
        vec![
            HighlightSpan {
                range: 0..4,
                kind: HighlightKind::Type
            },
            HighlightSpan {
                range: 4..8,
                kind: HighlightKind::Function
            },
            HighlightSpan {
                range: 8..10,
                kind: HighlightKind::Type
            },
        ]
    );
}

#[test]
fn an_unmapped_highlight_leaves_its_run_uncolored_and_the_stack_balanced() {
    // An out-of-range index pushes `None`; its source run is skipped, and a
    // following mapped run at the same depth still colors correctly (proving the
    // `None` did not corrupt the stack).
    let kw = name_index("keyword");
    let events = vec![
        Ok(HighlightEvent::HighlightStart(Highlight(9999))),
        Ok(HighlightEvent::Source { start: 0, end: 3 }),
        Ok(HighlightEvent::HighlightEnd),
        Ok(HighlightEvent::HighlightStart(Highlight(kw))),
        Ok(HighlightEvent::Source { start: 3, end: 5 }),
        Ok(HighlightEvent::HighlightEnd),
    ];
    let spans = spans_from_events(events.into_iter()).expect("no error");
    assert_eq!(
        spans,
        vec![HighlightSpan {
            range: 3..5,
            kind: HighlightKind::Keyword
        }]
    );
}

#[test]
fn an_empty_source_run_and_a_run_with_no_active_highlight_contribute_nothing() {
    let kw = name_index("keyword");
    let events = vec![
        // A source run outside any highlight.
        Ok(HighlightEvent::Source { start: 0, end: 4 }),
        // An empty run inside a highlight.
        Ok(HighlightEvent::HighlightStart(Highlight(kw))),
        Ok(HighlightEvent::Source { start: 4, end: 4 }),
        Ok(HighlightEvent::HighlightEnd),
    ];
    assert!(
        spans_from_events(events.into_iter())
            .expect("no error")
            .is_empty()
    );
}

#[test]
fn an_event_error_propagates() {
    // A parse cancellation surfaces as an error the caller must handle, not a
    // silently truncated span list.
    let events = vec![
        Ok(HighlightEvent::HighlightStart(Highlight(0))),
        Err(Error::Cancelled),
    ];
    assert!(spans_from_events(events.into_iter()).is_err());
}
