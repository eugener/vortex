//! The tree-sitter <-> core boundary for highlighting (SPEC §5) - the pure half.
//!
//! Everything that turns a grammar's raw highlight events into the core's own
//! [`HighlightSpan`]s lives here and is a plain function of its inputs, so the
//! whole mapping is testable with a real grammar but no channels or executor -
//! the same split [`crate::lsp::convert`] uses for the diagnostic boundary.

use tree_sitter_highlight::{Error, Highlight, HighlightEvent};

use crate::decoration::HighlightKind;
use crate::syntax::HighlightSpan;

/// The highlight names the producer recognizes, each paired with the
/// [`HighlightKind`] it maps to. The order *is* the contract:
/// [`tree_sitter_highlight::HighlightConfiguration::configure`] is handed
/// [`names`] in this order and later reports a matched highlight as
/// `Highlight(i)` - an index straight back into this table (see
/// [`kind_from_index`]).
///
/// A grammar's finer captures collapse to the nearest entry by
/// longest-prefix, which `configure` does for us: `@function.method` and
/// `@function.macro` both fall to `function` unless a more specific row exists
/// (`function.macro` does, so a macro stays distinct while a method does not).
/// `@comment.documentation` -> `comment`, `@variable.builtin` -> `variable`, and
/// so on. This is the granularity the theme styles.
const RECOGNIZED: [(&str, HighlightKind); 18] = [
    ("attribute", HighlightKind::Attribute),
    ("comment", HighlightKind::Comment),
    ("constant", HighlightKind::Constant),
    ("constant.builtin", HighlightKind::ConstantBuiltin),
    ("constructor", HighlightKind::Constructor),
    ("escape", HighlightKind::Escape),
    ("function", HighlightKind::Function),
    ("function.macro", HighlightKind::Macro),
    ("keyword", HighlightKind::Keyword),
    ("label", HighlightKind::Label),
    ("operator", HighlightKind::Operator),
    ("property", HighlightKind::Property),
    ("punctuation", HighlightKind::Punctuation),
    ("string", HighlightKind::String),
    ("type", HighlightKind::Type),
    ("type.builtin", HighlightKind::TypeBuiltin),
    ("variable", HighlightKind::Variable),
    ("variable.parameter", HighlightKind::Parameter),
];

/// The recognized highlight names in table order, for
/// [`tree_sitter_highlight::HighlightConfiguration::configure`].
pub(crate) fn names() -> Vec<&'static str> {
    RECOGNIZED.iter().map(|(name, _)| *name).collect()
}

/// The [`HighlightKind`] for a `Highlight(i)` the highlighter emitted - a lookup
/// into [`RECOGNIZED`], since `i` is an index into the names `configure` was
/// given. `None` only if the index is out of range, which cannot happen for a
/// configuration built from [`names`] but is handled rather than panicked on
/// (SPEC §8: never `unwrap` on producer output).
pub(crate) fn kind_from_index(i: usize) -> Option<HighlightKind> {
    RECOGNIZED.get(i).map(|(_, kind)| *kind)
}

/// Fold a highlighter's event stream into flat spans (SPEC §5).
///
/// The events are a nested stream - `HighlightStart(h)` ... `Source { .. }` ...
/// `HighlightEnd` - so the active highlight is a stack and the *innermost* one
/// colors each `Source` run (`function` inside `type` inside nothing: the name at
/// the top wins). An unmapped highlight pushes `None` so the stack stays balanced
/// against its `HighlightEnd`, and a `Source` under it is simply left uncolored
/// rather than borrowing the color of an enclosing span.
///
/// Empty `Source` runs (`start == end`) and runs with no active highlight
/// contribute nothing, so the output is only the spans a frontend actually paints.
pub(crate) fn spans_from_events(
    events: impl Iterator<Item = Result<HighlightEvent, Error>>,
) -> Result<Vec<HighlightSpan>, Error> {
    let mut stack: Vec<Option<HighlightKind>> = Vec::new();
    let mut spans = Vec::new();
    for event in events {
        match event? {
            HighlightEvent::HighlightStart(Highlight(i)) => stack.push(kind_from_index(i)),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if start < end
                    && let Some(Some(kind)) = stack.last()
                {
                    spans.push(HighlightSpan {
                        range: start..end,
                        kind: *kind,
                    });
                }
            }
        }
    }
    Ok(spans)
}

#[cfg(test)]
#[path = "highlight_tests.rs"]
mod tests;
