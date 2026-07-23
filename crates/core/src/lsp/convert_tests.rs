use super::*;
use crate::buffer::{Buffer, RopeBuffer};

/// The exact fixture the M2 spike fed rust-analyzer, whose byte / char / UTF-16
/// columns all differ (32 / 23 / 24 for the trailing `msg`).
const FIXTURE: &str = "pub fn bad() -> i32 {\n    let msg = \"日本語 😀\"; msg\n}\n";

fn lsp_diagnostic(
    (sl, sc): (u32, u32),
    (el, ec): (u32, u32),
    severity: Option<lsp_types::DiagnosticSeverity>,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range {
            start: lsp_types::Position::new(sl, sc),
            end: lsp_types::Position::new(el, ec),
        },
        severity,
        message: "mismatched types".into(),
        ..Default::default()
    }
}

#[test]
fn a_real_rust_analyzer_diagnostic_underlines_the_right_span() {
    // The milestone's actual acceptance criterion, pinned as a unit test: this is
    // verbatim what rust-analyzer published for FIXTURE in the M2 spike.
    let text = RopeBuffer::from(FIXTURE).text();
    let d = diagnostic(lsp_diagnostic(
        (1, 24),
        (1, 27),
        Some(lsp_types::DiagnosticSeverity::ERROR),
    ));
    let decorations = decorations_for(&text, &[d]);

    let underline = decorations
        .iter()
        .find_map(|d| match d {
            Decoration::Underline { range, .. } => Some(range.clone()),
            _ => None,
        })
        .expect("an error span underlines something");
    assert_eq!(
        text.slice(underline.clone()),
        "msg",
        "the underline must cover exactly the flagged identifier"
    );
    // Reading those same numbers as byte columns would have underlined `; ` -
    // the off-by-one this whole boundary exists to prevent.
    assert_ne!(underline.start, text.byte_of_line(1).unwrap() + 24);
}

#[test]
fn each_diagnostic_yields_an_underline_and_a_gutter_mark_on_its_line() {
    let text = RopeBuffer::from(FIXTURE).text();
    let d = diagnostic(lsp_diagnostic(
        (1, 24),
        (1, 27),
        Some(lsp_types::DiagnosticSeverity::ERROR),
    ));
    let decorations = decorations_for(&text, &[d]);
    assert_eq!(decorations.len(), 2);
    let mark = decorations
        .iter()
        .find_map(|d| match d {
            Decoration::GutterMark { offset, kind } => Some((*offset, *kind)),
            _ => None,
        })
        .expect("a gutter mark");
    assert_eq!(mark.1, GutterKind::Diagnostic(Severity::Error));
    assert_eq!(text.line_of_byte(mark.0), 1);
}

#[test]
fn severities_map_onto_the_semantic_tags() {
    use lsp_types::DiagnosticSeverity as S;
    assert_eq!(severity(Some(S::ERROR)), Severity::Error);
    assert_eq!(severity(Some(S::WARNING)), Severity::Warning);
    assert_eq!(severity(Some(S::INFORMATION)), Severity::Information);
    assert_eq!(severity(Some(S::HINT)), Severity::Hint);
}

#[test]
fn an_omitted_severity_is_taken_as_an_error() {
    // The LSP spec leaves this to the client; erring loud beats hiding a real
    // error as a hint.
    assert_eq!(severity(None), Severity::Error);
}

#[test]
fn a_zero_width_diagnostic_marks_the_gutter_but_underlines_nothing() {
    // "expected a token here" style diagnostics have an empty range; they must
    // still be visible somewhere.
    let text = RopeBuffer::from("let x = ;\n").text();
    let d = diagnostic(lsp_diagnostic((0, 8), (0, 8), None));
    let decorations = decorations_for(&text, &[d]);
    assert_eq!(decorations.len(), 1);
    assert!(matches!(decorations[0], Decoration::GutterMark { .. }));
}

#[test]
fn a_diagnostic_naming_a_line_the_buffer_lacks_is_dropped() {
    // A batch computed against a longer version of the file must not panic or
    // pin a squiggle to the last line (SPEC §8).
    let text = RopeBuffer::from("one line\n").text();
    let d = diagnostic(lsp_diagnostic((99, 0), (99, 4), None));
    assert!(decorations_for(&text, &[d]).is_empty());
}

#[test]
fn an_inverted_server_range_yields_no_underline_rather_than_an_inverted_slice() {
    // Defensive: an inverted range would panic a later slice, so only the gutter
    // mark survives.
    let text = RopeBuffer::from("abcdef\n").text();
    let d = diagnostic(lsp_diagnostic((0, 5), (0, 2), None));
    let decorations = decorations_for(&text, &[d]);
    assert_eq!(decorations.len(), 1);
    assert!(matches!(decorations[0], Decoration::GutterMark { .. }));
}

#[test]
fn a_multi_line_diagnostic_spans_across_the_line_break() {
    let text = RopeBuffer::from("fn f() {\n    bad\n}\n").text();
    let d = diagnostic(lsp_diagnostic((0, 3), (1, 7), None));
    let decorations = decorations_for(&text, &[d]);
    let underline = decorations
        .iter()
        .find_map(|d| match d {
            Decoration::Underline { range, .. } => Some(range.clone()),
            _ => None,
        })
        .expect("an underline");
    assert_eq!(text.slice(underline), "f() {\n    bad");
}

#[test]
fn several_diagnostics_all_convert() {
    let text = RopeBuffer::from(FIXTURE).text();
    let diagnostics = vec![
        diagnostic(lsp_diagnostic(
            (1, 24),
            (1, 27),
            Some(lsp_types::DiagnosticSeverity::ERROR),
        )),
        diagnostic(lsp_diagnostic(
            (0, 16),
            (0, 19),
            Some(lsp_types::DiagnosticSeverity::HINT),
        )),
    ];
    let decorations = decorations_for(&text, &diagnostics);
    assert_eq!(decorations.len(), 4); // two marks + two underlines
}

#[test]
fn the_message_survives_conversion() {
    let d = diagnostic(lsp_diagnostic((0, 0), (0, 1), None));
    assert_eq!(d.message, "mismatched types");
    assert_eq!(d.start, Utf16Position::new(0, 0));
    assert_eq!(d.end, Utf16Position::new(0, 1));
}
