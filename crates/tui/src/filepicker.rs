//! The file picker (SPEC §7.5) - a [`Picker`] over the files under a directory,
//! opened with Ctrl+F. Fuzzy-find a file by name instead of typing its full path.
//!
//! Like the command palette ([`crate::palette`]) it is a thin instance of the shared
//! [`Picker`]; it only supplies the item list - here, a bounded recursive walk of the
//! working directory. Each item's label is the path relative to the root (what you
//! filter on) and its command opens the *absolute* path, so the pick works whatever
//! the working directory is later. Picking runs the same `Command` dispatch as a key.

use std::fs;
use std::path::{Path, PathBuf};

use vortex_core::Action;

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::picker::{Item, Picker};

/// Cap on files collected, so a pathological tree cannot stall the walk or the
/// on-thread fuzzy match. Typical projects are far under this; a huge corpus wants
/// the async `nucleo` crate, deferred (see [`crate::picker`]).
const MAX_FILES: usize = 10_000;
/// Directory names skipped wholesale: build/vendor trees that bury source files.
/// Dot-entries (`.git`, …) are skipped separately by the leading-dot rule.
const IGNORE_DIRS: &[&str] = &["target", "node_modules"];

/// Collect files under `root`, as paths **relative to `root`**, sorted. Skips
/// dot-entries and [`IGNORE_DIRS`], does not follow symlinks (avoids cycles), and
/// stops at [`MAX_FILES`]. Unreadable directories are skipped rather than failing the
/// whole walk (SPEC §8: defensive I/O, no `unwrap` on the filesystem).
fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || IGNORE_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    files.push(rel.to_path_buf());
                }
                if files.len() >= MAX_FILES {
                    break;
                }
            }
        }
    }
    files.sort();
    files
}

/// Build the item list: one entry per file, labelled by its relative path, opening
/// the absolute path.
fn items(root: &Path) -> Vec<Item> {
    collect_files(root)
        .into_iter()
        .map(|rel| Item {
            label: rel.to_string_lossy().into_owned(),
            shortcut: None,
            command: Command::Editor(Action::Open(root.join(rel))),
        })
        .collect()
}

/// Open the file picker over `root`, styled from the theme.
pub fn open(theme: &Theme, root: &Path) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Open File",
        items(root),
        true, // path-aware fuzzy matching
        theme.palette,
        theme.palette_selected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn collects_files_recursively_relative_to_root() {
        let t = TempDir::new();
        t.file("a.txt", "");
        t.file("src/main.rs", "");
        t.file("src/nested/deep.rs", "");
        let mut found = collect_files(&t.path);
        found.sort();
        assert_eq!(
            found,
            vec![
                PathBuf::from("a.txt"),
                PathBuf::from("src/main.rs"),
                PathBuf::from("src/nested/deep.rs"),
            ]
        );
    }

    #[test]
    fn skips_dot_entries_and_ignored_dirs() {
        let t = TempDir::new();
        t.file("keep.rs", "");
        t.file(".hidden", ""); // dot-file
        t.file(".git/config", ""); // dot-dir
        t.file("target/debug/thing", ""); // ignored dir
        t.file("node_modules/pkg/index.js", ""); // ignored dir
        let found = collect_files(&t.path);
        assert_eq!(found, vec![PathBuf::from("keep.rs")]);
    }

    #[test]
    fn items_open_absolute_paths_with_relative_labels() {
        let t = TempDir::new();
        t.file("src/main.rs", "");
        let items = items(&t.path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "src/main.rs");
        assert_eq!(
            items[0].command,
            Command::Editor(Action::Open(t.path.join("src/main.rs")))
        );
    }

    #[test]
    fn empty_directory_yields_no_items() {
        let t = TempDir::new();
        assert!(collect_files(&t.path).is_empty());
    }
}
