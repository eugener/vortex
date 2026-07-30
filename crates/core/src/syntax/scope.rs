//! The tree-sitter <-> core boundary for structural scopes (SPEC §7.5, M8) - the
//! pure half, the sticky context header's twin of [`super::highlight`].
//!
//! Turning a parse tree into the flat [`ByteRange`]s the core publishes is a plain
//! function of its inputs, so the whole mapping is testable with a real grammar and
//! no channels or executor - the split [`crate::lsp::convert`] and
//! [`super::highlight`] both use.

use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::buffer::ByteRange;

/// The capture a context query marks a scope with. A grammar's `context.scm` is
/// free to use other captures for predicates and helper patterns (`@_name`, the
/// tree-sitter convention); only this one names a node whose first line the
/// frontend may pin.
pub(crate) const CONTEXT_CAPTURE: &str = "context";

/// Every scope `query` finds in `tree`, sorted by start with the outermost first
/// among scopes sharing one, and de-duplicated.
///
/// **A scope confined to one line is dropped.** The header exists to show a line
/// that has scrolled off the top, so a node that begins and ends on the same row
/// can never produce one: by the time its row is above the viewport its whole
/// extent is, and nothing about it encloses what is on screen. Filtering here
/// rather than in the frontend means a one-line `impl` block never crosses the
/// seam or costs an anchor transform per edit - and tree-sitter answers the
/// question for free, since every node carries its start and end row.
///
/// The sort is what makes the published bucket's order the header's order:
/// decorations are stored sorted by start (`DecorationSet::sorted_bucket`), and
/// a scope enclosing another starts no later than it, so iterating the bucket
/// yields outermost-first - which is the order the header rows are drawn in.
/// Ties (an `impl` and its sole `fn` opening at the same byte, which the
/// one-line filter above does not catch when both span many lines) are broken by
/// the wider range first, for the same reason.
pub(crate) fn scopes_from_tree(tree: &Tree, query: &Query, source: &[u8]) -> Vec<ByteRange> {
    // A query with no `@context` capture describes no scopes. Resolved once here
    // rather than compared per capture, since capture indices are what a match
    // actually carries.
    let Some(wanted) = query
        .capture_names()
        .iter()
        .position(|&name| name == CONTEXT_CAPTURE)
    else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source);
    let mut scopes: Vec<ByteRange> = Vec::new();
    // `QueryMatches` is a streaming iterator (tree-sitter 0.25+), so this is a
    // `while let` over `next()` rather than a `for`.
    while let Some(m) = matches.next() {
        for capture in m.captures.iter().filter(|c| c.index as usize == wanted) {
            let node = capture.node;
            if node.start_position().row < node.end_position().row {
                scopes.push(node.byte_range());
            }
        }
    }
    scopes.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    scopes.dedup();
    scopes
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
