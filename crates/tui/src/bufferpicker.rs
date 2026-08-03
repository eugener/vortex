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
use crate::config::{Config, Glyphs};
use crate::layout::{buffer_display_name, with_modified_marker};
use crate::picker::{Item, Picker, dir_columns, display_path};

/// One row per open buffer. The label is the file's *full* path where it has one
/// (not just the file name, which the bufferline shows): the picker is where you go
/// when several buffers share a name, so the thing you filter on has to be the thing
/// that tells them apart. It carries the modified marker too, so a dirty buffer is
/// as obvious here as on its tab.
fn items(buffers: &[BufferInfo], active: BufferId, glyphs: Glyphs) -> Vec<Item> {
    // Resolved once for the whole list rather than per row (see `display_path`).
    let cwd = std::env::current_dir().ok();
    buffers
        .iter()
        .map(|info| {
            let label = label_for(info, cwd.as_deref(), glyphs);
            Item {
                dim_columns: dir_columns(&label),
                label,
                // The active buffer is marked where a shortcut would go: picking it is a
                // no-op, and saying so beats leaving the current buffer unidentifiable.
                shortcut: (info.id == active).then(|| "current".to_string()),
                command: Command::Editor(Action::SwitchBuffer { id: info.id }),
            }
        })
        .collect()
}

/// A buffer's picker label: its path when it has one, otherwise the unnamed-buffer
/// placeholder, with any modified marker in front.
///
/// The caller asks [`dir_columns`] about the *whole* label rather than about the path
/// inside it, which is right because no mark in a [`Glyphs`] set is a path separator:
/// the label's last separator is the name's, shifted by the marker, and a row with no
/// directory has none either way. `every_mark_is_one_cell_wide_in_both_profiles` pins
/// that.
fn label_for(info: &BufferInfo, cwd: Option<&Path>, glyphs: Glyphs) -> String {
    let name = match info.path.as_deref() {
        Some(path) => display_path(path, cwd),
        None => buffer_display_name(None),
    };
    with_modified_marker(&name, info.modified, glyphs)
}

/// Open the buffer picker over the snapshot's buffer list.
pub fn open(config: &Config, buffers: &[BufferInfo], active: BufferId) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Switch Buffer",
        items(buffers, active, config.glyphs),
        true, // path-aware fuzzy matching: the labels are paths
        config,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use unicode_width::UnicodeWidthStr;

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
        let items = items(&list, BufferId(1), Glyphs::UNICODE);
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
        let items = items(&list, BufferId(2), Glyphs::UNICODE);
        assert_eq!(items[0].shortcut, None);
        assert_eq!(items[1].shortcut, Some("current".to_string()));
    }

    #[test]
    fn a_modified_buffer_carries_its_marker() {
        let list = [info(1, Some("/tmp/dirty.rs"), true)];
        assert!(
            items(&list, BufferId(9), Glyphs::UNICODE)[0]
                .label
                .starts_with("● ")
        );
    }

    #[test]
    fn an_unnamed_buffer_gets_the_placeholder_name() {
        let list = [info(1, None, false)];
        assert_eq!(
            items(&list, BufferId(1), Glyphs::UNICODE)[0].label,
            "[No Name]"
        );
        // …and still shows it is unsaved when it is.
        let dirty = [info(1, None, true)];
        assert_eq!(
            items(&dirty, BufferId(1), Glyphs::UNICODE)[0].label,
            "● [No Name]"
        );
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
        assert_eq!(
            items(&list, BufferId(1), Glyphs::UNICODE)[0].label,
            "src/deep/thing.rs"
        );
    }

    #[test]
    fn the_quiet_part_of_a_row_covers_the_marker_and_the_directory() {
        // The modified marker sits *before* the path, so the quiet run has to reach
        // over it: a row that dimmed only from `src/` would leave the marker and the
        // space after it loud, which reads as a stray highlight (SPEC §7.5, M10).
        let cwd = std::env::current_dir().unwrap();
        let nested = cwd.join("src/deep/thing.rs");
        let dirty = [info(1, Some(nested.to_str().unwrap()), true)];
        let row = &items(&dirty, BufferId(1), Glyphs::UNICODE)[0];
        assert_eq!(row.label, "● src/deep/thing.rs");
        assert_eq!(row.dim_columns, "* src/deep/".width());

        // A buffer with no path has no directory to quiet, marker or not.
        let unnamed = [info(1, None, true)];
        assert_eq!(
            items(&unnamed, BufferId(1), Glyphs::UNICODE)[0].dim_columns,
            0
        );
    }

    #[test]
    fn an_empty_buffer_list_yields_no_rows() {
        assert!(items(&[], BufferId(1), Glyphs::UNICODE).is_empty());
    }
}
