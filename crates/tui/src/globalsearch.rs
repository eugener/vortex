//! The global-search picker (SPEC §7.5, §14 M7) - find a pattern across the project
//! and jump to it.
//!
//! The fourth instance of the shared [`Picker`], and the first whose rows are not a
//! list it was handed. A file picker can enumerate its candidates before you type;
//! the matches for a pattern cannot exist before the pattern does, and then arrive
//! over a second or two of walking. So this drives the picker's [`ItemSource`] seam:
//! the query text *is* the search, results stream in through
//! [`crate::compositor::Layer::tick`], and each keystroke cancels the walk the last
//! one started ([`crate::search`]).
//!
//! **A pick is two actions, not one.** Opening the file is only half of arriving at
//! a match, so a row commits [`Command::OpenAt`], which the dispatcher turns into
//! `Open` followed by `PlaceCursorAt`. Both go down the same channel to the same
//! actor in order, so the jump resolves against the buffer the open just produced -
//! no waiting for a snapshot, and no position arithmetic on this side.

use std::path::{Path, PathBuf};

use vortex_core::Position;

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::picker::{Item, ItemSource, Picker, PreviewSource};
use crate::search::{self, Hit, Search};

/// Lines of context shown above the matched line in the preview pane. A match with
/// nothing before it reads as the top of a file rather than as a place in one.
const CONTEXT_BEFORE: usize = 2;

/// The picker's rows, driven by [`crate::search`].
///
/// Holds the root (each query starts a fresh walk of it) and the running search, if
/// any. Dropping the old [`Search`] is what cancels it, so replacing this field is
/// the whole of "stop the last query".
struct Matches {
    root: PathBuf,
    running: Option<Search>,
    /// Set when the query is not a valid regex: shown where the rows would be, since
    /// otherwise a typo and a genuine no-match look exactly alike.
    error: Option<String>,
    /// Whether a search is under way, so "nothing yet" can be told from "nothing".
    searching: bool,
}

impl ItemSource for Matches {
    fn query(&mut self, query: &str) {
        // Dropping the previous search cancels its walk (see `search::Search`).
        self.running = None;
        self.error = None;
        self.searching = false;
        // An empty query is not a search for everything. It is also the state the
        // picker opens in, and walking the project before a pattern exists would
        // spend the whole first second of it on results nobody asked for.
        if query.is_empty() {
            return;
        }
        match search::spawn(&self.root, query) {
            Ok(search) => {
                self.running = Some(search);
                self.searching = true;
            }
            Err(err) => self.error = Some(first_line(&err.to_string())),
        }
    }

    fn take(&mut self) -> Vec<Item> {
        let Some(search) = self.running.as_mut() else {
            return Vec::new();
        };
        search.drain().iter().map(row).collect()
    }

    fn status(&self) -> Option<String> {
        match (&self.error, self.searching) {
            (Some(err), _) => Some(err.clone()),
            (None, true) => Some("searching…".to_string()),
            (None, false) => None,
        }
    }
}

/// A hit as a picker row: `path:line` and the matched text, committing the jump.
///
/// The path is shown relative to the working directory when it is under it - the
/// rows are narrow, and the part that tells two hits apart is the end of the path,
/// not the machine it is on.
fn row(hit: &Hit) -> Item {
    Item {
        label: format!(
            "{}:{}  {}",
            display_path(&hit.path),
            hit.line + 1, // 1-based for the eye; the Position stays 0-based
            hit.text
        ),
        shortcut: None,
        command: Command::OpenAt {
            path: hit.path.clone(),
            position: Position::new(hit.line, hit.column),
        },
    }
}

/// The path as shown: relative to the working directory when it is under it,
/// absolute otherwise - the same rule the buffer picker's rows follow.
fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// `regex`'s errors are several lines of caret diagram; a picker row is one line.
fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("invalid pattern")
        .trim()
        .to_string()
}

/// The pane's source: the matched line in its surroundings, which is the question a
/// list of `path:line` rows leaves open - one line of a match rarely says whether it
/// is the one you meant.
fn preview_source() -> PreviewSource {
    Box::new(|item, lines| match &item.command {
        Command::OpenAt { path, position } => crate::filepicker::preview_around(
            path,
            position.line.saturating_sub(CONTEXT_BEFORE),
            lines,
        ),
        _ => Vec::new(),
    })
}

/// Open the global-search picker over `root`.
pub fn open(theme: &Theme, root: &Path) -> Box<dyn Layer> {
    let matches = Matches {
        root: root.to_path_buf(),
        running: None,
        error: None,
        searching: false,
    };
    Box::new(
        Picker::new(
            "Search Project",
            Vec::new(), // every row arrives from the source
            false,      // the query is a regex, not a fuzzy pattern - no path tuning
            theme.palette,
            theme.palette_selected,
        )
        .with_item_source(Box::new(matches))
        .with_preview_pane(preview_source()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use ratatui::layout::Rect;

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 24,
    };

    fn matches(root: &Path) -> Matches {
        Matches {
            root: root.to_path_buf(),
            running: None,
            error: None,
            searching: false,
        }
    }

    /// Query and then drain until the walk finishes. The worker is a thread, so a
    /// single `take` would race it.
    fn run(source: &mut Matches, query: &str) -> Vec<Item> {
        source.query(query);
        let mut items = Vec::new();
        // The search either has a running walk or never started one.
        while source.running.is_some() {
            let batch = source.take();
            let done = source
                .running
                .as_ref()
                .is_some_and(|s| s.is_finished() && batch.is_empty());
            items.extend(batch);
            if done {
                break;
            }
        }
        items
    }

    #[test]
    fn a_hit_becomes_a_row_that_opens_the_file_at_the_match() {
        let t = TempDir::new();
        t.file("src/a.rs", "one\ntwo\nlet needle = 1;\n");
        let mut source = matches(&t.path);
        let items = run(&mut source, "needle");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].command,
            Command::OpenAt {
                path: t.path.join("src/a.rs"),
                // 0-based line 2 is the third line; column 4 is past "let ".
                position: Position::new(2, 4),
            }
        );
        assert!(
            items[0].label.ends_with(":3  let needle = 1;"),
            "the row shows a 1-based line and the matched text: {:?}",
            items[0].label
        );
    }

    #[test]
    fn an_empty_query_searches_nothing() {
        // The state the picker opens in. Walking the project before a pattern exists
        // would spend the first second of it on results nobody asked for.
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        source.query("");
        assert!(source.running.is_none());
        assert!(source.take().is_empty());
        assert_eq!(source.status(), None);
    }

    #[test]
    fn a_new_query_replaces_the_last_ones_results() {
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        t.file("b.rs", "haystack\n");
        let mut source = matches(&t.path);
        assert_eq!(run(&mut source, "needle").len(), 1);
        let second = run(&mut source, "haystack");
        assert_eq!(second.len(), 1);
        assert!(second[0].label.contains("haystack"));
    }

    #[test]
    fn a_bad_pattern_is_shown_on_the_row_rather_than_searched() {
        // A typo and a genuine no-match look identical in an empty box.
        let t = TempDir::new();
        t.file("a.rs", "x\n");
        let mut source = matches(&t.path);
        source.query("unclosed(");
        assert!(source.running.is_none(), "nothing was searched");
        let status = source.status().expect("the error is shown");
        assert!(!status.contains('\n'), "one line only: {status:?}");
        assert!(!status.is_empty());
        // …and it clears once the pattern is valid again.
        source.query("unclosed");
        assert_eq!(source.status().as_deref(), Some("searching…"));
    }

    #[test]
    fn the_status_says_a_search_is_running_before_any_row_arrives() {
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        source.query("needle");
        assert_eq!(source.status().as_deref(), Some("searching…"));
    }

    #[test]
    fn the_preview_shows_the_match_in_its_surroundings() {
        // A list of path:line rows leaves open the question the pane answers: one
        // line of a match rarely says whether it is the one you meant.
        let t = TempDir::new();
        t.file("a.rs", "line0\nline1\nline2\nline3\nNEEDLE\nline5\nline6\n");
        let item = row(&Hit {
            path: t.path.join("a.rs"),
            line: 4,
            column: 0,
            text: "NEEDLE".to_string(),
        });
        let shown = preview_source()(&item, 5);
        assert_eq!(
            shown,
            vec!["line2", "line3", "NEEDLE", "line5", "line6"],
            "two lines of context above the match"
        );
    }

    #[test]
    fn a_match_near_the_top_previews_from_the_first_line() {
        // The context window cannot run off the top of the file.
        let t = TempDir::new();
        t.file("a.rs", "first\nsecond\nthird\n");
        let item = row(&Hit {
            path: t.path.join("a.rs"),
            line: 0,
            column: 0,
            text: "first".to_string(),
        });
        assert_eq!(preview_source()(&item, 3), vec!["first", "second", "third"]);
    }

    #[test]
    fn a_row_that_is_not_a_jump_previews_nothing() {
        let item = Item {
            label: "x".to_string(),
            shortcut: None,
            command: Command::OpenPalette,
        };
        assert!(preview_source()(&item, 4).is_empty());
    }

    #[test]
    fn the_picker_opens_with_a_pane_and_no_rows() {
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let layer = open(&Theme::default(), &t.path);
        let mut buf = ratatui::buffer::Buffer::empty(SCREEN);
        layer.render(SCREEN, &mut buf);
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(shown.contains("Search Project"), "titled");
        assert!(
            !shown.contains("needle"),
            "nothing is searched until something is typed"
        );
    }

    #[test]
    fn a_path_outside_the_working_directory_stays_absolute() {
        assert_eq!(display_path(Path::new("/etc/hosts")), "/etc/hosts");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(display_path(&cwd.join("src/x.rs")), "src/x.rs");
    }
}
