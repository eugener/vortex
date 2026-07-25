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
use crate::layout::buffer_display_name;
use crate::picker::{Item, Picker};

/// One row per open buffer. The label is the file's *full* path where it has one
/// (not just the file name, which the bufferline shows): the picker is where you go
/// when several buffers share a name, so the thing you filter on has to be the thing
/// that tells them apart. The modified marker rides along from
/// [`buffer_display_name`], so a dirty buffer is as obvious here as on its tab.
fn items(buffers: &[BufferInfo], active: BufferId) -> Vec<Item> {
    buffers
        .iter()
        .map(|info| Item {
            label: label_for(info),
            // The active buffer is marked where a shortcut would go: picking it is a
            // no-op, and saying so beats leaving the current buffer unidentifiable.
            shortcut: (info.id == active).then(|| "current".to_string()),
            command: Command::Editor(Action::SwitchBuffer { id: info.id }),
        })
        .collect()
}

/// A buffer's picker label: its path when it has one, otherwise the unnamed-buffer
/// placeholder, with any modified marker.
fn label_for(info: &BufferInfo) -> String {
    match info.path.as_deref() {
        Some(path) => decorate(&display_path(path), info.modified),
        None => buffer_display_name(None, info.modified),
    }
}

/// The path as shown: relative to the working directory when it is under it, so the
/// rows stay short in the common case of editing within one project, and absolute
/// otherwise (a buffer opened from elsewhere must not read as a local file).
fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Prefix the modified marker, matching [`buffer_display_name`]'s convention.
fn decorate(name: &str, modified: bool) -> String {
    if modified {
        format!("● {name}")
    } else {
        name.to_string()
    }
}

/// Open the buffer picker over the snapshot's buffer list.
pub fn open(theme: &Theme, buffers: &[BufferInfo], active: BufferId) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Switch Buffer",
        items(buffers, active),
        true, // path-aware fuzzy matching: the labels are paths
        theme.palette,
        theme.palette_selected,
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
    fn a_path_under_the_working_directory_is_shown_relative() {
        // Long absolute paths would push the distinguishing part off the row, which
        // is the opposite of what a picker over same-named files is for.
        let cwd = std::env::current_dir().unwrap();
        let nested = cwd.join("src/deep/thing.rs");
        assert_eq!(display_path(&nested), "src/deep/thing.rs");
    }

    #[test]
    fn a_path_outside_the_working_directory_stays_absolute() {
        // It must not read as a local file when it is not one.
        let outside = PathBuf::from("/etc/hosts");
        assert_eq!(display_path(&outside), "/etc/hosts");
    }

    #[test]
    fn an_empty_buffer_list_yields_no_rows() {
        assert!(items(&[], BufferId(1)).is_empty());
    }
}
