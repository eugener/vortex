//! The buffer picker (SPEC §7.5) - a [`Picker`] over the open buffers, for jumping
//! straight to one by name instead of stepping through the bufferline.
//!
//! The third instance of the shared [`Picker`], after the command palette and the
//! file picker, and the one with no I/O at all: its item list comes from the buffer
//! list the snapshot already carries, so opening it costs a walk of a handful of
//! entries rather than a directory scan. Picking commits an [`Action::SwitchBuffer`],
//! the same intent a bufferline key resolves to.

use std::path::Path;

use vortex_core::{Action, BufferId, BufferInfo};

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::layout::{buffer_display_name, with_modified_marker};
use crate::picker::{Item, Picker, display_path};

/// One row per open buffer. The label is the file's *full* path where it has one
/// (not just the file name, which the bufferline shows): the picker is where you go
/// when several buffers share a name, so the thing you filter on has to be the thing
/// that tells them apart. The modified marker rides along from
/// [`buffer_display_name`], so a dirty buffer is as obvious here as on its tab.
fn items(buffers: &[BufferInfo], active: BufferId) -> Vec<Item> {
    // Resolved once for the whole list rather than per row (see `display_path`).
    let cwd = std::env::current_dir().ok();
    buffers
        .iter()
        .map(|info| Item {
            label: label_for(info, cwd.as_deref()),
            // The active buffer is marked where a shortcut would go: picking it is a
            // no-op, and saying so beats leaving the current buffer unidentifiable.
            shortcut: (info.id == active).then(|| "current".to_string()),
            command: Command::Editor(Action::SwitchBuffer { id: info.id }),
        })
        .collect()
}

/// A buffer's picker label: its path when it has one, otherwise the unnamed-buffer
/// placeholder, with any modified marker.
fn label_for(info: &BufferInfo, cwd: Option<&Path>) -> String {
    let Some(path) = info.path.as_deref() else {
        return buffer_display_name(None, info.modified);
    };
    with_modified_marker(&display_path(path, cwd), info.modified)
}

/// Open the buffer picker over the snapshot's buffer list.
pub fn open(theme: &Theme, buffers: &[BufferInfo], active: BufferId) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Switch Buffer",
        items(buffers, active),
        true, // path-aware fuzzy matching: the labels are paths
        theme,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn info(id: u64, path: Option<&str>, modified: bool) -> BufferInfo {
        BufferInfo {
            id: BufferId(id),
            path: path.map(PathBuf::from),
            modified,
        }
    }

    #[test]
    fn each_buffer_becomes_a_row_that_switches_to_it() {
        let list = [info(1, Some("/tmp/a.rs"), false), info(2, None, false)];
        let items = items(&list, BufferId(1));
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].command,
            Command::Editor(Action::SwitchBuffer { id: BufferId(1) })
        );
        assert_eq!(
            items[1].command,
            Command::Editor(Action::SwitchBuffer { id: BufferId(2) })
        );
    }

    #[test]
    fn the_active_buffer_is_marked_as_current() {
        let list = [
            info(1, Some("/tmp/a.rs"), false),
            info(2, Some("/b.rs"), false),
        ];
        let items = items(&list, BufferId(2));
        assert_eq!(items[0].shortcut, None);
        assert_eq!(items[1].shortcut, Some("current".to_string()));
    }

    #[test]
    fn a_modified_buffer_carries_its_marker() {
        let list = [info(1, Some("/tmp/dirty.rs"), true)];
        assert!(items(&list, BufferId(9))[0].label.starts_with("● "));
    }

    #[test]
    fn an_unnamed_buffer_gets_the_placeholder_name() {
        let list = [info(1, None, false)];
        assert_eq!(items(&list, BufferId(1))[0].label, "[No Name]");
        // …and still shows it is unsaved when it is.
        let dirty = [info(1, None, true)];
        assert_eq!(items(&dirty, BufferId(1))[0].label, "● [No Name]");
    }

    #[test]
    fn a_row_shows_its_path_relative_to_the_working_directory() {
        // Long absolute paths would push the distinguishing part off the row, which
        // is the opposite of what a picker over same-named files is for. The rule
        // itself is `picker::display_path`'s; what this pins is that the rows are
        // built against the working directory rather than against nothing.
        let cwd = std::env::current_dir().unwrap();
        let nested = cwd.join("src/deep/thing.rs");
        let list = [info(1, Some(nested.to_str().unwrap()), false)];
        assert_eq!(items(&list, BufferId(1))[0].label, "src/deep/thing.rs");
    }

    #[test]
    fn an_empty_buffer_list_yields_no_rows() {
        assert!(items(&[], BufferId(1)).is_empty());
    }
}
