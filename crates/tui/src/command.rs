//! A frontend command: what a key binding or a picked palette entry *does* (SPEC
//! §7.5 UI-commit vocabulary).
//!
//! Either a core editor intent forwarded to the actor, or a frontend-local effect
//! (open an overlay) that never crosses the seam. This is the single type the event
//! loop dispatches, whether the command came from the keymap ([`crate::keymap`]) or
//! from a compositor layer committing a choice ([`crate::compositor::Layer`]) - so a
//! bound key and a palette selection run through the exact same path.

use vortex_core::{Action, BufferId, BufferInfo};

/// A dispatchable frontend command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Forward a core intent to the editor actor.
    Editor(Action),
    /// Open the command palette overlay (frontend-local).
    OpenPalette,
    /// Open the file picker overlay (frontend-local).
    OpenFilePicker,
    /// Open the theme picker overlay (frontend-local).
    OpenThemePicker,
    /// Open the buffer picker overlay (frontend-local). Its rows come from the
    /// snapshot's buffer list, so opening it needs no round trip; a pick commits an
    /// [`Action::SwitchBuffer`].
    OpenBufferPicker,
    /// Open the save-as prompt overlay (frontend-local). The prompt commits an
    /// [`Action::SaveAs`] for the typed path, so the target never crosses the seam
    /// until it is chosen (SPEC §7.5).
    OpenSavePrompt,
    /// Focus the next buffer, wrapping at the end of the bufferline. Resolved into an
    /// [`Action::SwitchBuffer`] at dispatch, where the snapshot's ordered buffer list
    /// is in hand: the core has no notion of "next", only of *which* buffer (§7.5).
    NextBuffer,
    /// Focus the previous buffer, wrapping at the start. See [`Command::NextBuffer`].
    PrevBuffer,
    /// Close the active buffer. Sent unforced, so the core refuses when there is
    /// unsaved work and the frontend turns that refusal into a confirmation.
    CloseBuffer,
    /// Swap the gutter between absolute and relative numbering (frontend-local).
    ///
    /// It mutates the live [`crate::config::Config`] rather than a paint-only flag,
    /// which is what the theme picker already does to swap themes mid-session: the
    /// config value *is* the running setting, and the file only says what it starts
    /// as. Like a theme pick, this lasts the session and is not written back to disk.
    ToggleLineNumbers,
    /// Draw or stop drawing the indent guides (frontend-local), the same live-config
    /// switch [`Command::ToggleLineNumbers`] is.
    ToggleIndentGuides,
    /// Show or hide the scrollbar (frontend-local). Unlike the other chrome toggles
    /// this one changes the *layout* - the reserved column comes and goes with it -
    /// but still only on this side of the seam: the core has no idea a viewport
    /// exists, let alone a column standing for one.
    ToggleScrollbar,
    /// Show or hide the sticky context header (frontend-local). Like the scrollbar
    /// this one changes the *layout* - the pinned rows come and go with it - and the
    /// data it pins is already on the snapshot, so nothing about it crosses the seam
    /// either.
    ToggleStickyContext,
    /// Open the encoding picker overlay (frontend-local). A pick commits an
    /// [`Action::SetEncoding`], so the choice reaches the core the same way a
    /// bound key's would.
    OpenEncodingPicker,
    /// Open the line-ending picker overlay (frontend-local), the twin of
    /// [`Command::OpenEncodingPicker`].
    OpenLineEndingPicker,
    /// Open the global-search picker overlay (frontend-local). Its rows come from a
    /// worker thread rather than a list, so unlike the other openers the surface it
    /// opens keeps working after it is on screen (`Layer::tick`).
    OpenSearchPicker,
    /// Open `path` and put the caret at `position` - a global-search hit.
    ///
    /// One command rather than two so a picker row can commit a whole arrival: it
    /// dispatches to [`Action::Open`] followed by [`Action::PlaceCursorAt`], both
    /// down the same channel in order, so the jump resolves against the buffer the
    /// open just produced. The jump carries `path` too, so an open that fails drops
    /// it rather than moving the caret in an unrelated buffer. Carries data, so like
    /// [`Command::SetTheme`] it is emitted by its picker rather than bound to a key.
    OpenAt {
        path: std::path::PathBuf,
        position: vortex_core::Position,
    },
    /// Open the in-buffer find prompt (frontend-local), or the find-and-replace one
    /// when `replacing` (SPEC §11). Seeded with the last pattern, so reopening offers
    /// it back rather than making the user retype it.
    OpenFindPrompt {
        replacing: bool,
    },
    /// The find prompt's pattern as it now stands - emitted on **every keystroke**,
    /// unlike every other command here, which is what makes the preview live.
    ///
    /// It never reaches the core. The frontend holds the buffer's text and owns the
    /// viewport, so highlighting the matches and scrolling to the next one are both
    /// answerable locally (SPEC §5, §7.5) - and a search the user then cancels has
    /// therefore changed nothing at all.
    PreviewSearch {
        pattern: String,
        replacement: String,
    },
    /// Forget the current search: highlights down, pattern gone. Escape, and the only
    /// gesture that means "done searching" - closing the prompt by committing keeps
    /// the query so find-next has something to repeat.
    ClearSearch,
    /// Go to the next (or previous) match of the remembered pattern - the find-next
    /// key, and what the query-replace walk advances with. Resolved against the
    /// frontend's own memory of the search at dispatch: with no pattern yet it is a
    /// no-op rather than a round trip, the same shape as [`Command::NextBuffer`].
    FindNext,
    FindPrevious,
    /// Put a cursor on every match of the remembered pattern - the multi-cursor
    /// gesture that turns a search into an edit.
    SelectAllMatches,
    /// Begin the query-replace walk over the current query (SPEC §11): the surface
    /// that asks what to do with each match in turn. Committed by the replace
    /// prompt, which is where both halves of the query were typed.
    StartReplace,
    /// Switch to the named theme (frontend-local: chrome never crosses the seam).
    ///
    /// Carries data, so unlike the openers above it is not a bindable
    /// [`crate::keymap::Command`] - it is only ever emitted by the theme picker,
    /// which is where the names come from.
    SetTheme(String),
}

/// The buffer `offset` positions from `active` in `buffers`, wrapping at both ends -
/// how [`Command::NextBuffer`] and [`Command::PrevBuffer`] become a concrete
/// `SwitchBuffer` (SPEC §7.5: the frontend owns the ordering, the core owns the
/// buffers).
///
/// `None` when there is nothing to move to: an empty list, an `active` that is not in
/// it (a snapshot older than a close), or a single buffer - where "next" would
/// re-select what is already on screen, so the key is better as a no-op than as a
/// pointless round trip.
pub fn neighbor_buffer(
    buffers: &[BufferInfo],
    active: BufferId,
    offset: isize,
) -> Option<BufferId> {
    if buffers.len() < 2 {
        return None;
    }
    let index = buffers.iter().position(|info| info.id == active)?;
    // `rem_euclid` so a negative offset wraps to the end rather than going negative.
    let wrapped = (index as isize + offset).rem_euclid(buffers.len() as isize) as usize;
    Some(buffers[wrapped].id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffers(ids: &[u64]) -> Vec<BufferInfo> {
        ids.iter()
            .map(|&id| BufferInfo {
                id: BufferId(id),
                path: None,
                modified: false,
            })
            .collect()
    }

    #[test]
    fn next_and_previous_step_through_the_list() {
        let list = buffers(&[10, 20, 30]);
        assert_eq!(neighbor_buffer(&list, BufferId(10), 1), Some(BufferId(20)));
        assert_eq!(neighbor_buffer(&list, BufferId(20), 1), Some(BufferId(30)));
        assert_eq!(neighbor_buffer(&list, BufferId(30), -1), Some(BufferId(20)));
    }

    #[test]
    fn stepping_past_either_end_wraps_around() {
        // A bufferline is a ring: next from the last is the first, and previous from
        // the first is the last.
        let list = buffers(&[10, 20, 30]);
        assert_eq!(neighbor_buffer(&list, BufferId(30), 1), Some(BufferId(10)));
        assert_eq!(neighbor_buffer(&list, BufferId(10), -1), Some(BufferId(30)));
    }

    #[test]
    fn a_lone_buffer_has_no_neighbor() {
        // Switching to the buffer already on screen is not worth a round trip.
        let list = buffers(&[10]);
        assert_eq!(neighbor_buffer(&list, BufferId(10), 1), None);
        assert_eq!(neighbor_buffer(&list, BufferId(10), -1), None);
        assert_eq!(neighbor_buffer(&[], BufferId(10), 1), None);
    }

    #[test]
    fn an_active_buffer_missing_from_the_list_yields_nothing() {
        // A snapshot older than a close can name a buffer no longer listed; that is a
        // no-op, not a panic or an arbitrary pick.
        let list = buffers(&[10, 20]);
        assert_eq!(neighbor_buffer(&list, BufferId(999), 1), None);
    }
}
