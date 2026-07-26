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
    /// Open the encoding picker overlay (frontend-local). A pick commits an
    /// [`Action::SetEncoding`], so the choice reaches the core the same way a
    /// bound key's would.
    OpenEncodingPicker,
    /// Open the line-ending picker overlay (frontend-local), the twin of
    /// [`Command::OpenEncodingPicker`].
    OpenLineEndingPicker,
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
