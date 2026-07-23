//! The LSP <-> core position-space boundary (SPEC §4).
//!
//! Every conversion between the server's line + UTF-16-code-unit positions and
//! the core's byte offsets happens in this file and nowhere else. Everything
//! here is a pure function of its inputs, so the whole boundary is testable
//! without a language server - which matters, because this is precisely where
//! the "underline is one column off" class of bug lives.

use async_lsp::lsp_types;

use crate::buffer::{Text, Utf16Position};
use crate::decoration::{Decoration, GutterKind, Severity};
use crate::lsp::Diagnostic;

/// An `lsp_types` position in the core's named UTF-16 space (SPEC §4).
///
/// LSP `u32` line/character values become `usize`; a server sending values
/// beyond the buffer is handled downstream by clamping, not here.
fn position(p: lsp_types::Position) -> Utf16Position {
    Utf16Position::new(p.line as usize, p.character as usize)
}

/// Map an LSP severity onto the core's semantic tag.
///
/// The LSP spec leaves an omitted severity to the client's interpretation; we
/// take it as an error, the conservative reading - under-reporting a real error
/// as a hint hides it, while the reverse is merely loud.
fn severity(s: Option<lsp_types::DiagnosticSeverity>) -> Severity {
    match s {
        Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Information,
        Some(lsp_types::DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::Error,
    }
}

/// Translate a server's diagnostic into the core's own [`Diagnostic`], keeping
/// its positions in UTF-16 space (they become byte offsets only against buffer
/// text, in [`decorations_for`]).
pub(crate) fn diagnostic(d: lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        start: position(d.range.start),
        end: position(d.range.end),
        severity: severity(d.severity),
        message: d.message,
    }
}

/// Resolve `diagnostics` against `text` into the decorations the frontend paints
/// (SPEC §5): an underline over the flagged span plus a gutter mark on its line.
///
/// **Which text?** The buffer's *current* content, not the version the server
/// analyzed. When the user has not typed since, they are the same and the result
/// is exact. When they have, the diagnostic is already stale and will be
/// replaced by the server's next publish - and converting against current text is
/// the closest available approximation in the meantime, since it at least lands
/// on a real position. Pinning the analyzed version instead would mean holding
/// old text plus an edit log to replay, which buys accuracy only inside a window
/// that self-heals (SPEC §5: overlays may trail text by a frame).
///
/// Defensive throughout (SPEC §8): a position naming a line the buffer does not
/// have is dropped rather than panicking, and a server sending an inverted range
/// yields a gutter mark with no underline instead of an inverted slice.
pub(crate) fn decorations_for(text: &Text, diagnostics: &[Diagnostic]) -> Vec<Decoration> {
    let mut out = Vec::with_capacity(diagnostics.len() * 2);
    for d in diagnostics {
        // A line past the end of the buffer is not clampable to anything
        // meaningful, so drop the diagnostic rather than pin it to the last line.
        let (Some(start), Some(end)) = (
            text.byte_of_utf16_position(d.start),
            text.byte_of_utf16_position(d.end),
        ) else {
            continue;
        };
        // The gutter mark goes on the span's *start* line and is emitted even for
        // an empty span - a zero-width diagnostic (a missing token, an unexpected
        // EOF) underlines nothing but must still be visible somewhere.
        out.push(Decoration::GutterMark {
            offset: start,
            kind: GutterKind::Diagnostic(d.severity),
        });
        if end > start {
            out.push(Decoration::Underline {
                range: start..end,
                severity: d.severity,
            });
        }
    }
    out
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
