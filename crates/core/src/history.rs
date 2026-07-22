//! Undo history as a tree, with structural coalescing (SPEC §2.4).
//!
//! **Tree, not stack** (SPEC §2.4): undo-then-type on a stack destroys the redo
//! branch and loses that work. A tree keeps every branch reachable - typing after
//! an undo forks a new branch instead of clearing the old one. Navigation is
//! linear for now (undo -> parent, redo -> newest child); the preserved branches
//! are what a later `:earlier`/`:later` time-travel rides on, so that is an
//! additive feature, not a refactor.
//!
//! **Generic over any action.** History does not know about `Insert` vs `Delete`
//! vs a future paste: it records [`Change`]s (byte range + removed text + inserted
//! text), which every edit already produces. One user action - even fanned across
//! N cursors - is one [`Revision`], the unit undo/redo moves over (SPEC §2.4).
//!
//! **Selections travel with the edit.** A revision stores the selection set on
//! each side, so undo restores the pre-edit carets and redo the post-edit ones -
//! the cursor lands where the user expects, not at offset 0.
//!
//! **Coalescing** merges a run of single-caret character inserts into one undo
//! unit (without it, undo reverts one keystroke at a time - unusable). The run is
//! broken by (b) a non-adjacent edit, (c) a newline, or (d) a cursor/selection
//! change (the editor calls [`History::break_coalescing`] on any motion). SPEC
//! §2.4's rule (a) - a time gap - needs a clock and is deferred; the frontend can
//! force a boundary later without changing this shape.

use std::ops::Range;

use crate::selection::SelectionSet;

/// One atomic text change: replace `removed` (the old content) at `start` with
/// `inserted`. Enough to redo (apply `inserted` over `removed`'s span) and undo
/// (apply `removed` over `inserted`'s span). `start` is a byte offset in the
/// buffer *before* the change applies (its parent revision's pre-edit buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    /// Byte offset where the change begins, in pre-edit (parent) coordinates.
    pub start: usize,
    /// Text that was there before (removed by the change). Empty for a pure insert.
    pub removed: String,
    /// Text inserted in its place. Empty for a pure deletion.
    pub inserted: String,
}

/// One reversible edit group - the unit undo/redo moves over. `changes` are sorted
/// ascending by `start` and are disjoint (they came from a disjoint selection set,
/// SPEC §2.2). `before`/`after` are the selection sets to restore on undo/redo.
#[derive(Debug, Clone)]
struct Revision {
    changes: Vec<Change>,
    before: SelectionSet,
    after: SelectionSet,
}

/// A node in the undo tree. The root (index 0) has no `revision` - it is the state
/// the document started in (empty, or a freshly loaded file). Every other node's
/// `revision` transforms its parent's buffer into this node's buffer.
#[derive(Debug, Clone)]
struct Node {
    /// The edit that produced this node from its parent; `None` only for the root.
    revision: Option<Revision>,
    /// Parent index; `None` only for the root.
    parent: Option<usize>,
    /// Children in creation order. Redo follows the last (most recent) child, so
    /// the newest branch is the default while older branches stay reachable.
    children: Vec<usize>,
}

/// The undo tree plus a cursor into it (`current`) and the saved-state marker.
#[derive(Debug, Clone)]
pub(crate) struct History {
    nodes: Vec<Node>,
    /// The node whose buffer is currently live.
    current: usize,
    /// The node matching the on-disk file, if any. `modified` is derived as
    /// `current != saved` (SPEC §8), so undoing back to the saved state clears the
    /// modified marker.
    saved: Option<usize>,
    /// Whether the next character insert may coalesce into `current`'s revision.
    /// Cleared by [`Self::break_coalescing`] (a motion) and by any non-insert edit.
    coalescing: bool,
}

/// The edits to apply for an undo or redo, plus the selection set to restore
/// afterward. `edits` are `(range, replacement)` pairs against the *current*
/// buffer, pre-sorted DESCENDING by start so back-to-front application is
/// offset-stable (the same convention as `editor::plan_edit`).
pub(crate) struct Reverted {
    pub edits: Vec<(Range<usize>, String)>,
    pub selections: SelectionSet,
}

impl History {
    /// A fresh history rooted at the current (empty or just-loaded) state, which is
    /// the saved state. Used at startup and on every file open (an open discards
    /// undo history - you cannot undo across loading a different file).
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                revision: None,
                parent: None,
                children: Vec::new(),
            }],
            current: 0,
            saved: Some(0),
            coalescing: false,
        }
    }

    /// Record an applied edit as a new revision (or coalesce it into the current
    /// one). `changes` need not be sorted; `before`/`after` are the selection sets
    /// bracketing the edit. Coalescing merges a single-caret character insert that
    /// sits immediately after the previous such insert (SPEC §2.4).
    pub fn record(&mut self, mut changes: Vec<Change>, before: SelectionSet, after: SelectionSet) {
        changes.sort_by_key(|c| c.start);

        // Whether *this* edit is a single typed grapheme decides if a run stays open
        // for the NEXT keystroke. Computed from the incoming edit (before it is moved
        // into the node) rather than by re-inspecting the resulting node: a coalesced
        // typing node ("ab") and a freshly recorded multi-char paste ("hello") are
        // indistinguishable as nodes (both single-change pure inserts), but only the
        // former should leave the run open. A paste closes the run so the next typed
        // character starts its own undo unit (SPEC §2.4).
        let opens_run = is_typed_grapheme(&changes);

        // Try to fold this into the current revision first, from the borrowed pieces
        // - on the hot single-caret typing path this avoids building a `Revision`
        // (and re-cloning `after`/the inserted text) only to drop it.
        if self.coalescing && self.try_coalesce(&changes, &after) {
            // Extended the current node's revision in place; `current` unchanged.
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Node {
                revision: Some(Revision {
                    changes,
                    before,
                    after,
                }),
                parent: Some(self.current),
                children: Vec::new(),
            });
            self.nodes[self.current].children.push(idx);
            self.current = idx;
        }

        // A single typed character opens (or continues) a coalescing run; anything
        // else - a paste, a delete, a multi-cursor edit - closes it, so the next
        // insert starts a fresh revision.
        self.coalescing = opens_run;
    }

    /// Try to fold a new edit (`changes` + its post-edit `after` selections) into the
    /// current node's revision. Succeeds only when both are single-caret character
    /// inserts and the new one begins exactly where the current one ends (adjacency).
    /// On success the current revision's inserted text grows and its `after`
    /// selections advance.
    fn try_coalesce(&mut self, changes: &[Change], after: &SelectionSet) -> bool {
        // The *incoming* edit must be a single typed grapheme (not a multi-char paste),
        // so a paste immediately after typing does not fold into the typing run.
        if !is_typed_grapheme(changes) {
            return false;
        }
        let new_change = &changes[0];
        let Some(current_revision) = self.nodes[self.current].revision.as_ref() else {
            return false; // at the root: nothing to coalesce into
        };
        if !changes_are_coalescable(&current_revision.changes) {
            return false;
        }
        let current_change = &current_revision.changes[0];
        // Adjacent iff the new insert starts right after the current insert's text.
        // Both starts are numerically comparable: no motion happened between them
        // (coalescing is set), so the buffer only grew by the current insert.
        if new_change.start != current_change.start + current_change.inserted.len() {
            return false;
        }

        // Extend in place. `changes`/`after` are separate borrows from `self.nodes`,
        // so the new text appends without cloning it out of anything first.
        let current = self.nodes[self.current].revision.as_mut().unwrap();
        current.changes[0].inserted.push_str(&changes[0].inserted);
        current.after = after.clone();
        true
    }

    /// Move to the parent, returning the edits that undo the current revision and
    /// the selections to restore. `None` at the root (nothing to undo).
    pub fn undo(&mut self) -> Option<Reverted> {
        let node = &self.nodes[self.current];
        let parent = node.parent?;
        // A non-root node always carries a revision.
        let revision = node
            .revision
            .as_ref()
            .expect("non-root node has a revision");
        let edits = inverse_edits(&revision.changes);
        let selections = revision.before.clone();
        self.current = parent;
        self.coalescing = false;
        Some(Reverted { edits, selections })
    }

    /// Move to the newest child, returning the edits that redo its revision and the
    /// selections to restore. `None` when the current node has no children (nothing
    /// to redo on this branch).
    pub fn redo(&mut self) -> Option<Reverted> {
        let child = *self.nodes[self.current].children.last()?;
        let revision = self.nodes[child]
            .revision
            .as_ref()
            .expect("a child node has a revision");
        let edits = forward_edits(&revision.changes);
        let selections = revision.after.clone();
        self.current = child;
        self.coalescing = false;
        Some(Reverted { edits, selections })
    }

    /// Mark the current node as the on-disk state (called on a successful save).
    /// After this, `at_saved` is true until the buffer moves off this node.
    ///
    /// Also ends any coalescing run: a saved node must stay immutable (its content
    /// is what is on disk), so the next character insert must start a *new*
    /// revision instead of coalescing into - and thereby mutating - the saved one.
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.current);
        self.coalescing = false;
    }

    /// Whether the live buffer matches the saved state - the basis for the
    /// `modified` marker (SPEC §8). False once no node is saved (e.g. a buffer
    /// never written that has been edited away from its root).
    pub fn at_saved(&self) -> bool {
        self.saved == Some(self.current)
    }

    /// End the current coalescing run so the next insert starts a new revision.
    /// The editor calls this on any cursor motion / selection change (SPEC §2.4
    /// break rule (d)).
    pub fn break_coalescing(&mut self) {
        self.coalescing = false;
    }
}

/// Whether a node's revision is an **open typing run** a further keystroke may extend:
/// exactly one change, a pure insert (nothing removed), and no newline (break rule
/// (c)). One change also implies a single caret, so multi-cursor typing does not
/// coalesce across time - each keystroke stays its own (still one) undo unit. This is
/// the *loose* check: it accepts a node that has already accumulated several typed
/// characters ("abc"), which is exactly what the current run holds after a few
/// keystrokes. The *incoming* edit is gated more tightly by [`is_typed_grapheme`].
fn changes_are_coalescable(changes: &[Change]) -> bool {
    changes.len() == 1 && changes[0].removed.is_empty() && !changes[0].inserted.contains('\n')
}

/// Whether an incoming edit is a single typed grapheme - the *only* kind that may open
/// or extend a coalescing run. Stricter than [`changes_are_coalescable`]: it also
/// requires the inserted text to be exactly one grapheme cluster, so a multi-character
/// insert - a paste, a bracketed paste (`Event::Paste` -> one `Insert`), or a snippet -
/// is its own undo unit and one Undo never reverts both the paste and the typing before
/// it (SPEC §2.4: a paste is one distinct action, not part of the typing run). A typed
/// character is one grapheme even when multi-byte (`é`, an emoji), so ordinary typing
/// still coalesces.
fn is_typed_grapheme(changes: &[Change]) -> bool {
    use unicode_segmentation::UnicodeSegmentation;
    if !changes_are_coalescable(changes) {
        return false;
    }
    let mut graphemes = changes[0].inserted.graphemes(true);
    // Exactly one grapheme: a first one exists and there is no second.
    graphemes.next().is_some() && graphemes.next().is_none()
}

/// Forward edits to REDO `changes`: re-apply each as originally done. `changes` are
/// ascending in parent-buffer coordinates, which are exactly the coordinates of the
/// buffer a redo applies to (the parent). Returned descending by start so
/// back-to-front application stays offset-stable.
fn forward_edits(changes: &[Change]) -> Vec<(Range<usize>, String)> {
    let mut edits: Vec<(Range<usize>, String)> = changes
        .iter()
        .map(|c| (c.start..c.start + c.removed.len(), c.inserted.clone()))
        .collect();
    // `changes` are sorted ascending (record() sorts them), so descending is a
    // plain reversal - no re-sort needed.
    edits.reverse();
    edits
}

/// Inverse edits to UNDO `changes`: replace each inserted span with what it
/// removed. `changes` are ascending in parent coordinates, but an undo applies to
/// the *child* buffer, where earlier siblings shifted later offsets - so shift each
/// start by the running net length change before emitting it. Returned descending
/// by start for offset-stable back-to-front application.
fn inverse_edits(changes: &[Change]) -> Vec<(Range<usize>, String)> {
    let mut shift: isize = 0;
    let mut edits: Vec<(Range<usize>, String)> = Vec::with_capacity(changes.len());
    for change in changes {
        let child_start = (change.start as isize + shift) as usize;
        edits.push((
            child_start..child_start + change.inserted.len(),
            change.removed.clone(),
        ));
        shift += change.inserted.len() as isize - change.removed.len() as isize;
    }
    // Shifted starts stay ascending (disjoint ascending changes shift each child
    // start monotonically), so descending is a plain reversal - no re-sort needed.
    edits.reverse();
    edits
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
