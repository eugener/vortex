//! In-buffer search: turning a regex into places in the text (SPEC §11, §12.2).
//!
//! The other half of the search story. Cross-file search is frontend-owned
//! (`vortex-tui::search`) because a walk of a project is filesystem work; this half
//! lives in the core because its answers are *selections* over the rope the core
//! owns. `select-all-matches` and `split-on-regex` (SPEC §12.2) are selection
//! operations, and a frontend computing them would be re-deriving the buffer's own
//! text to hand back offsets into it.
//!
//! **Matches never span a line.** The pattern is applied to one line's content at a
//! time, without its terminator, which is the same rule the cross-file walk follows
//! and the same rule `grep` follows. It is also what keeps the cost of a search
//! bounded by the lines it touches rather than by the file (SPEC §10.4): a
//! whole-buffer scan is opt-in per caller through the line range, so the frontend's
//! live highlight can ask for the viewport alone and pay for nothing else. `^` and
//! `$` therefore anchor to a *line*, not the buffer, which is what a user typing
//! `^fn ` means.
//!
//! **Smart case**, matching the cross-file search exactly: an all-lowercase pattern
//! is case-insensitive (typing lowercase says you do not care), and one uppercase
//! letter anywhere makes the whole pattern case-sensitive (typing a capital says you
//! did). The two searches share the rule because a user does not hold two mental
//! models of what their own typing means.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::buffer::Text;

/// Compile `pattern` with smart case (SPEC §11).
///
/// # Errors
/// Returns `regex`'s own compile error for a pattern that is not a valid regex. A
/// pattern is *user input*, so every caller in the core reports this rather than
/// unwrapping it (SPEC §8).
pub fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()
}

/// Every match on the lines in `lines`, as byte ranges in the whole buffer, in
/// ascending order.
///
/// The range is the caller's cost control: a live highlight passes the visible
/// lines, `select-all-matches` passes them all. Out-of-range line indices are
/// skipped rather than refused, so a caller holding a viewport computed against a
/// slightly older snapshot gets the lines that do exist instead of nothing.
///
/// **Empty matches are skipped.** A pattern like `x*` matches the empty string at
/// every position; selecting all of them would put a cursor between every pair of
/// characters and replacing them would splice text into every gap. Neither is what
/// the user meant, and no editor does it - so a match that covers no bytes is not a
/// place.
pub fn matches_in(text: &Text, regex: &Regex, lines: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for line_index in lines {
        let Some(content) = text.line(line_index) else {
            continue;
        };
        // `byte_of_line` agrees with `line` on the same index, so the unwrap is a
        // formality; 0 is a harmless fallback rather than a panic (SPEC §8).
        let start = text.byte_of_line(line_index).unwrap_or(0);
        out.extend(
            regex
                .find_iter(&content)
                .filter(|m| !m.is_empty())
                .map(|m| start + m.start()..start + m.end()),
        );
    }
    out
}

/// The first match at or after byte `from`, searching forward and wrapping to the
/// start of the buffer - or, `backward`, the last match strictly before `from`,
/// wrapping to the end.
///
/// `None` only when the pattern matches nowhere in the buffer: with the wrap, a
/// pattern that matches anywhere always has a next match, even when the caret is
/// already sitting on the only one.
///
/// The forward search is lazy line by line and stops at the first hit, so finding
/// the next match in a large file costs the lines between here and there rather
/// than the file - the reason this is not written as a filter over
/// [`matches_in`] of the whole buffer. The backward search scans the lines behind
/// the caret the same way, taking the last match on each.
///
/// Forward is "at or after `from`" and backward is "strictly before `from`" on
/// purpose: repeatedly finding forward from the *start* of the current match would
/// never leave it, so the caller advances - but a match found forward and then
/// searched backward from its own start has to be able to reach the previous one,
/// not itself.
pub fn next_match(text: &Text, regex: &Regex, from: usize, backward: bool) -> Option<Range<usize>> {
    let last = text.last_line_index();
    let (line, col) = {
        let pos = text.position_of_byte(from);
        (pos.line, pos.col)
    };
    if backward {
        // Behind the caret on its own line, then every earlier line, then wrap
        // around from the end. The caret's line is visited **twice** - once bounded
        // by the column and once whole at the end of the wrap - which is what lets a
        // backward search from column 0 reach a match later on that same line
        // instead of stopping one short of the full circle.
        let earlier = (0..=line).rev().chain((line..=last).rev());
        for (step, index) in earlier.enumerate() {
            // Only the caret's own line is bounded by the column - and only on the
            // first visit, since the wrap comes back around to it whole.
            let limit = (step == 0).then_some(col);
            if let Some(found) = line_matches(text, regex, index, limit, true) {
                return Some(found);
            }
        }
    } else {
        let later = (line..=last).chain(0..=line.min(last));
        for (step, index) in later.enumerate() {
            let limit = (step == 0).then_some(col);
            if let Some(found) = line_matches(text, regex, index, limit, false) {
                return Some(found);
            }
        }
    }
    None
}

/// The first match on `line_index` at or after column `limit` (or the last one
/// strictly before it, `backward`), as a whole-buffer byte range. `limit` of `None`
/// searches the whole line.
fn line_matches(
    text: &Text,
    regex: &Regex,
    line_index: usize,
    limit: Option<usize>,
    backward: bool,
) -> Option<Range<usize>> {
    let content = text.line(line_index)?;
    let start = text.byte_of_line(line_index).unwrap_or(0);
    // A column can be past the content when the caret sits on the terminator.
    let limit = limit.map(|col| col.min(content.len()));
    let mut found = regex.find_iter(&content).filter(|m| !m.is_empty());
    let hit = if backward {
        found
            .take_while(|m| limit.is_none_or(|col| m.start() < col))
            .last()?
    } else {
        found.find(|m| limit.is_none_or(|col| m.start() >= col))?
    };
    Some(start + hit.start()..start + hit.end())
}

/// Every match on the lines in `lines`, paired with the text that replacing it with
/// `template` produces - the edit list a replace applies (SPEC §11).
///
/// `template` expands `$1` / `$name` capture references against each match.
/// Expansion is what makes a regex *replace* worth having over a literal one:
/// `(\w+)\s+(\w+)` → `$2 $1` is most of the reason to type a pattern with groups in
/// it. The syntax is `regex`'s own (`Captures::expand`), not one invented here, and
/// a reference to a group that did not participate expands to nothing, which is
/// `regex`'s rule too. A `$` meant literally is written `$$`.
///
/// Captures are taken **on the line**, in the same pass that finds the match, rather
/// than by re-running the pattern over the matched text afterwards. Those are not the
/// same thing: `a|ab` matching `ab` in context would re-match as just `a` on its own,
/// and the groups an anchored pattern captured would be gone. One pass is both
/// cheaper and the only correct one.
///
/// Ranges come back ascending, so the caller reverses them once to apply
/// right-to-left rather than re-sorting per edit.
pub fn replacements_in(
    text: &Text,
    regex: &Regex,
    lines: Range<usize>,
    template: &str,
) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    for line_index in lines {
        let Some(content) = text.line(line_index) else {
            continue;
        };
        let start = text.byte_of_line(line_index).unwrap_or(0);
        for caps in regex.captures_iter(&content) {
            // Group 0 is always present - it is the match itself.
            let Some(whole) = caps.get(0).filter(|m| !m.is_empty()) else {
                continue;
            };
            let mut replacement = String::new();
            caps.expand(template, &mut replacement);
            out.push((start + whole.start()..start + whole.end(), replacement));
        }
    }
    out
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
