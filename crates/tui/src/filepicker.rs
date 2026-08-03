//! The file picker (SPEC §7.5) - a [`Picker`] over the files under a directory,
//! opened with Ctrl+F. Fuzzy-find a file by name instead of typing its full path.
//!
//! Like the command palette ([`crate::palette`]) it is a thin instance of the shared
//! [`Picker`]; it supplies the item list - here, a bounded recursive walk of the
//! working directory - and, unlike the others, a **preview pane**: a path is not a
//! file, and the whole point of fuzzy-finding by name is that you are not certain
//! which one you meant. Each item's label is the path relative to the root (what you
//! filter on) and its command opens the *absolute* path, so the pick works whatever
//! the working directory is later. Picking runs the same `Command` dispatch as a key.

use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use vortex_core::Action;

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Config;
use crate::picker::{Item, Picker, PreviewSource, dir_columns};

/// Cap on files collected, so a pathological tree cannot stall the walk or the
/// on-thread fuzzy match. Typical projects are far under this; a huge corpus wants
/// the async `nucleo` crate, deferred (see [`crate::picker`]).
const MAX_FILES: usize = 10_000;
/// Directory names skipped wholesale: build/vendor trees that bury source files.
/// Dot-entries (`.git`, …) are skipped separately by the leading-dot rule.
const IGNORE_DIRS: &[&str] = &["target", "node_modules"];
/// Bytes read to fill the preview pane. The pane shows a screenful at most, so
/// reading a whole file to paint sixteen lines of it would put an unbounded read on
/// the keystroke that moved the highlight - and holding Down through a directory of
/// them would stall the picker entirely.
const PREVIEW_BYTES: u64 = 16 * 1024;
/// Bytes read when the preview starts partway down the file - a search hit, which
/// can be anywhere in it. Matched to [`crate::search`]'s own file cap: the searcher
/// will not report a hit in a file bigger than this, so a read this long always
/// reaches the line it is being asked about.
const DEEP_PREVIEW_BYTES: u64 = 1024 * 1024;

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
        .map(|rel| {
            let label = rel.to_string_lossy().into_owned();
            Item {
                dim_columns: dir_columns(&label),
                label,
                shortcut: None,
                command: Command::Editor(Action::Open(root.join(rel))),
            }
        })
        .collect()
}

/// The pane's source. A row's *command* is what names the file, not its label: the
/// label is a lossy rendering of the path, so a filename that is not valid UTF-8
/// would preview as unreadable while picking it opened fine.
fn preview_source() -> PreviewSource {
    Box::new(|item, lines| match &item.command {
        Command::Editor(Action::Open(path)) => preview(path, lines),
        _ => Vec::new(),
    })
}

/// The first `lines` lines of `path`, or the one line that says why there are none.
///
/// Decoded with the *core's* loader, so the pane shows what opening the file would
/// show - a preview that guessed the encoding differently would misrepresent the
/// thing it exists to let you identify. A binary file is named rather than shown, the
/// same refusal `Action::Open` makes on it (SPEC §10.3).
fn preview(path: &Path, lines: usize) -> Vec<String> {
    preview_around(path, 0, lines)
}

/// The `lines` lines of `path` starting at 0-based `first_line`, or the one line
/// that says why there are none.
///
/// The window exists for the global-search picker, whose rows name a place in a file
/// rather than a file ([`crate::globalsearch`]) - a preview of the top of the file
/// would answer a question nobody asked. Starting anywhere but the top costs a
/// longer read ([`DEEP_PREVIEW_BYTES`]), since the line has to be reached before it
/// can be shown.
pub fn preview_around(path: &Path, first_line: usize, lines: usize) -> Vec<String> {
    let cap = if first_line == 0 {
        PREVIEW_BYTES
    } else {
        DEEP_PREVIEW_BYTES
    };
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| file.take(cap).read_to_end(&mut bytes));
    if let Err(err) = read {
        return vec![format!("cannot read: {err}")];
    }
    if vortex_core::file::is_binary(&bytes) {
        return vec!["binary file".to_string()];
    }
    trim_cut_tail(&mut bytes, cap);
    let text = vortex_core::file::load(&bytes).text;
    text.lines()
        .skip(first_line)
        .take(lines)
        .map(clean)
        .collect()
}

/// Undo what stopping at [`PREVIEW_BYTES`] did to the end of the read.
///
/// It stops mid-line, and can stop mid-*character* - and a cut multi-byte sequence is
/// not something [`vortex_core::file::load`] can distinguish from a file that is not
/// UTF-8 at all, so it would fall back to windows-1252 and render the whole preview
/// as mojibake over one truncated character. Cutting back to the last line break
/// removes both the half character and the half line; a file with no line break in
/// 16 KiB (a minified one) has only the character to fix.
fn trim_cut_tail(bytes: &mut Vec<u8>, cap: u64) {
    if bytes.len() as u64 != cap {
        return;
    }
    if let Some(newline) = bytes.iter().rposition(|&b| b == b'\n') {
        bytes.truncate(newline + 1);
    } else if let Err(err) = std::str::from_utf8(bytes) {
        // Only an *incomplete* tail is the cut's doing. Bytes that are invalid UTF-8
        // on their own merits are the file's own, and windows-1252 is there for them.
        if err.error_len().is_none() {
            bytes.truncate(err.valid_up_to());
        }
    }
}

/// A line as the pane can paint it. A tab is not a tab stop in a cell buffer, it is
/// one broken cell, and the other control characters are worse - so the tab becomes
/// the spaces it stands for and the rest go.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Open the file picker over `root`, styled from the theme.
pub fn open(config: &Config, root: &Path) -> Box<dyn Layer> {
    Box::new(
        Picker::new(
            "Open File",
            items(root),
            true, // path-aware fuzzy matching
            config,
        )
        .with_preview_pane(preview_source()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::send;
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

    #[test]
    fn preview_shows_the_first_lines_of_the_file() {
        let t = TempDir::new();
        t.file("a.rs", "fn main() {\n    body();\n}\nafter\n");
        assert_eq!(
            preview(&t.path.join("a.rs"), 3),
            vec!["fn main() {", "    body();", "}"],
            "no more lines than the pane asked for"
        );
    }

    #[test]
    fn preview_expands_tabs_and_drops_other_control_characters() {
        // A raw tab in a cell buffer is one broken cell, not a tab stop.
        let t = TempDir::new();
        t.file("a.rs", "\tif x {\u{7} }\n");
        assert_eq!(preview(&t.path.join("a.rs"), 4), vec!["    if x { }"]);
    }

    #[test]
    fn preview_names_a_binary_file_instead_of_showing_it() {
        // The same refusal `Action::Open` makes: a PNG is not text, and windows-1252
        // would happily decode it into a screenful of mojibake (SPEC §10.3).
        let t = TempDir::new();
        t.file("blob.png", "\u{0}\u{1}\u{2}PNG");
        assert_eq!(preview(&t.path.join("blob.png"), 4), vec!["binary file"]);
    }

    #[test]
    fn preview_of_an_unreadable_path_says_so() {
        let t = TempDir::new();
        let lines = preview(&t.path.join("gone.rs"), 4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("cannot read: "), "{lines:?}");
    }

    #[test]
    fn preview_decodes_the_way_opening_the_file_would() {
        // windows-1252, the fallback the core keeps a nameless encoding in: the pane
        // has to agree with it, or it misrepresents the file it exists to identify.
        let t = TempDir::new();
        std::fs::write(t.path.join("latin.txt"), b"caf\xe9\n").unwrap();
        assert_eq!(preview(&t.path.join("latin.txt"), 4), vec!["café"]);
    }

    #[test]
    fn preview_of_a_long_file_stops_at_a_line_boundary() {
        // The read stops at 16 KiB, which lands mid-line; the fragment must not be
        // painted as if it were a line of its own.
        let t = TempDir::new();
        let body = "0123456789abcdef\n".repeat(2000); // ~34 KiB
        t.file("long.txt", &body);
        let lines = preview(&t.path.join("long.txt"), 6);
        assert_eq!(lines, vec!["0123456789abcdef"; 6]);
        // The cut is invisible even at the very end of what was read.
        let all = preview(&t.path.join("long.txt"), 10_000);
        assert!(
            all.iter().all(|l| l == "0123456789abcdef"),
            "cut line shown"
        );
    }

    #[test]
    fn preview_does_not_mistake_a_cut_character_for_another_encoding() {
        // The regression this guards: a multi-byte character straddling the read cap
        // is malformed UTF-8, which `load` cannot tell from a file that was never
        // UTF-8 - so it re-decoded the *whole* preview as windows-1252 mojibake.
        // One line, so there is no newline to cut back to.
        let t = TempDir::new();
        t.file("wide.txt", &format!("a{}", "é".repeat(10_000)));
        let lines = preview(&t.path.join("wide.txt"), 4);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("aéé"),
            "decoded as latin-1: {:?}",
            &lines[0][..8]
        );
        assert!(
            !lines[0].contains('\u{fffd}'),
            "the cut character was kept as a replacement"
        );
    }

    #[test]
    fn the_preview_source_reads_the_path_the_row_opens() {
        // Not the label: that is a lossy rendering of the path, so a filename that is
        // not valid UTF-8 would preview as unreadable while picking it opened fine.
        let t = TempDir::new();
        t.file("src/main.rs", "fn main() {}\n");
        let items = items(&t.path);
        assert_eq!(preview_source()(&items[0], 4), vec!["fn main() {}"]);
        // A row that opens nothing has nothing to show, rather than a guess at one.
        let other = Item {
            label: "src/main.rs".to_string(),
            dim_columns: 0,
            shortcut: None,
            command: Command::OpenPalette,
        };
        assert!(preview_source()(&other, 4).is_empty());
    }

    #[test]
    fn preview_leaves_a_file_that_is_simply_not_utf8_to_the_core() {
        // One long line of windows-1252 and no line break to cut back to: the tail is
        // invalid on the file's own merits, not because the read stopped there, so
        // trimming it would hide most of the file behind the first accented byte.
        let t = TempDir::new();
        let mut body = Vec::new();
        while body.len() < 20_000 {
            body.extend_from_slice(b"caf\xe9 ");
        }
        std::fs::write(t.path.join("latin.txt"), &body).unwrap();
        let lines = preview(&t.path.join("latin.txt"), 4);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("café café"), "{:?}", &lines[0][..12]);
        assert!(lines[0].chars().count() > 1_000, "cut back to the first é");
    }

    #[test]
    fn preview_around_windows_the_file_at_a_line() {
        // The global-search picker's rows name a place in a file, not a file: a
        // preview of the top would answer a question nobody asked.
        let t = TempDir::new();
        t.file("a.rs", "l0\nl1\nl2\nl3\nl4\nl5\n");
        assert_eq!(
            preview_around(&t.path.join("a.rs"), 2, 3),
            vec!["l2", "l3", "l4"]
        );
        // Past the end yields nothing rather than erroring or wrapping.
        assert!(preview_around(&t.path.join("a.rs"), 99, 3).is_empty());
    }

    #[test]
    fn preview_around_reaches_a_line_past_the_shallow_read_cap() {
        // A hit can be anywhere in a file the searcher would read, which is far past
        // the 16 KiB a preview-from-the-top needs. The deeper cap is what makes the
        // window reachable at all - with the shallow one this line is never read.
        let t = TempDir::new();
        let filler = "填".repeat(20); // 60 bytes a line, so line 1000 is past 16 KiB
        let mut body: String = (0..1000).map(|_| format!("{filler}\n")).collect();
        body.push_str("the deep line\n");
        t.file("deep.rs", &body);
        assert!(body.len() > PREVIEW_BYTES as usize);
        assert_eq!(
            preview_around(&t.path.join("deep.rs"), 1000, 1),
            vec!["the deep line"]
        );
    }

    #[test]
    fn the_open_file_picker_carries_a_pane() {
        // The wiring: `open` is what a keypress reaches, and a picker built without
        // the source would silently be the old list-only one.
        let t = TempDir::new();
        t.file("a.rs", "fn main() {}\n");
        let mut layer = open(&Config::default(), &t.path);
        let screen = ratatui::layout::Rect::new(0, 0, 100, 24);
        let mut buf = ratatui::buffer::Buffer::empty(screen);
        layer.render(screen, &mut buf);
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(shown.contains("fn main() {}"), "no preview painted");
        // …and the row it previews is still the row it opens.
        send(
            &mut *layer,
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Enter,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::Open(t.path.join("a.rs")))]
        );
    }
}
