//! The global-search picker (SPEC §7.5, §14 M7) - find a pattern across the project
//! and jump to it.
//!
//! The fourth instance of the shared [`Picker`], and the first whose rows are not a
//! list it was handed. A file picker can enumerate its candidates before you type;
//! the matches for a pattern cannot exist before the pattern does, and then arrive
//! over a second or two of walking. So this drives the picker's [`ItemSource`] seam:
//! the query text *is* the search, results stream in through
//! [`crate::compositor::Layer::tick`], and a keystroke cancels whatever the last one
//! started ([`crate::search`]).
//!
//! **A keystroke does not start a walk; standing still does.** Typing a word would
//! otherwise be a walk per letter, each cancelled by the next a file too late (see
//! [`DEBOUNCE`]). The wait needs a clock the picker does not have - so it uses the
//! one it does: [`ItemSource::take`] is called once per render tick, and that is
//! where the pending query comes due.
//!
//! **A pick is two actions, not one.** Opening the file is only half of arriving at
//! a match, so a row commits [`Command::OpenAt`], which the dispatcher turns into
//! `Open` followed by `PlaceCursorAt`. Both go down the same channel to the same
//! actor in order, so the jump resolves against the buffer the open just produced -
//! no waiting for a snapshot, and no position arithmetic on this side. The jump
//! names the file it was measured against, because the open can fail: a hit is only
//! as fresh as the walk that found it, and an unguarded jump would then land in
//! whatever buffer happened to be focused.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vortex_core::Position;

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::picker::{Item, ItemSource, Picker, PreviewSource, display_path};
use crate::search::{self, Hit, Search};

/// Lines of context shown above the matched line in the preview pane. A match with
/// nothing before it reads as the top of a file rather than as a place in one.
const CONTEXT_BEFORE: usize = 2;

/// How long a query must stand still before it is walked.
///
/// Typing `needle` is six queries, and without this each one starts a walk that the
/// next cancels - but cancellation is only checked between files, so a slow tree does
/// a file's worth of redundant work per keystroke. The wait is the same 150ms
/// `main.rs` already holds a frame for syntax highlights (`HIGHLIGHT_WAIT`): long
/// enough to swallow a burst of typing, short enough that a pause reads as instant.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Where the current query has got to - one field rather than a flag per outcome,
/// so "failed" and "still running" cannot both be true and the status line has
/// exactly one thing to say.
///
/// Every state but [`Self::Idle`] says something, because an empty box under a typed
/// query is the one thing a picker cannot explain by itself: a typo, a search still
/// walking, and a genuine no-match look identical without it.
#[derive(Debug, PartialEq, Eq)]
enum State {
    /// Nothing has been asked for - the state the picker opens in.
    Idle,
    /// Typed and waiting out [`DEBOUNCE`], or walking. The wait counts as searching:
    /// it is 150ms of a search the user has already asked for.
    Searching,
    /// The walk ended. Only ever *shown* when no rows arrived, since the picker puts
    /// the status where the rows would be - so this reads as "and found nothing".
    Done,
    /// The query is not a valid regex. Carries the message, one line of it.
    Failed(String),
}

/// The picker's rows, driven by [`crate::search`].
///
/// Holds the root (each query starts a fresh walk of it) and the running search, if
/// any. Dropping the old [`Search`] is what cancels it, so replacing this field is
/// the whole of "stop the last query".
struct Matches {
    root: PathBuf,
    /// The working directory as of when the picker opened, for rendering each row's
    /// path relative to it. Resolved once rather than per row: a walk can deliver a
    /// thousand hits, and the working directory cannot change under a modal overlay.
    cwd: Option<PathBuf>,
    running: Option<Search>,
    /// A query typed but not yet walked, and the moment it comes due. Replaced by
    /// the next keystroke, so only the pattern you stopped on is ever searched.
    pending: Option<(String, Instant)>,
    state: State,
}

impl Matches {
    /// [`ItemSource::query`] against a caller-supplied clock, which is what makes the
    /// wait testable without one.
    fn restart(&mut self, query: &str, now: Instant) {
        // Dropping the previous search cancels its walk (see `search::Search`).
        self.running = None;
        self.pending = None;
        self.state = State::Idle;
        // An empty query is not a search for everything. It is also the state the
        // picker opens in, and walking the project before a pattern exists would
        // spend the whole first second of it on results nobody asked for.
        if query.is_empty() {
            return;
        }
        self.pending = Some((query.to_string(), now + DEBOUNCE));
        self.state = State::Searching;
    }

    /// Start the pending walk once its query has stood still for [`DEBOUNCE`].
    ///
    /// A bad pattern is reported here rather than at the keystroke: typing `(foo)`
    /// passes through `(`, and flashing that error on the way is noise about a
    /// pattern nobody had finished writing.
    fn due(&mut self, now: Instant) {
        let Some((pattern, _)) = self.pending.take_if(|(_, due)| now >= *due) else {
            return;
        };
        match search::spawn(&self.root, &pattern) {
            Ok(search) => self.running = Some(search),
            Err(err) => self.state = State::Failed(first_line(&err.to_string())),
        }
    }
}

impl ItemSource for Matches {
    fn query(&mut self, query: &str) {
        self.restart(query, Instant::now());
    }

    fn take(&mut self) -> Vec<Item> {
        // Called once per render tick, which is also the clock the wait needs: the
        // picker has no other heartbeat, and a keystroke is exactly what must *not*
        // be what starts the walk.
        self.due(Instant::now());
        let Some(search) = self.running.as_mut() else {
            return Vec::new();
        };
        let hits = search.drain();
        // The end of the walk is only observable through a drain, so this is the one
        // place that can retire "searching…" - and it has to, or a query that matches
        // nothing claims to still be looking for it, forever.
        if search.is_finished() {
            self.state = State::Done;
        }
        hits.iter()
            .map(|hit| row(hit, self.cwd.as_deref()))
            .collect()
    }

    fn status(&self) -> Option<Cow<'_, str>> {
        match &self.state {
            State::Idle => None,
            State::Searching => Some(Cow::Borrowed("searching…")),
            State::Done => Some(Cow::Borrowed("no matches")),
            State::Failed(err) => Some(Cow::Borrowed(err)),
        }
    }
}

/// A hit as a picker row: `path:line` and the matched text, committing the jump.
///
/// The path is shown relative to `cwd` when it is under it - the rows are narrow,
/// and the part that tells two hits apart is the end of the path, not the machine
/// it is on.
fn row(hit: &Hit, cwd: Option<&Path>) -> Item {
    Item {
        label: format!(
            "{}:{}  {}",
            display_path(&hit.path, cwd),
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
        cwd: std::env::current_dir().ok(),
        running: None,
        pending: None,
        state: State::Idle,
    };
    Box::new(
        Picker::new(
            "Search Project",
            Vec::new(), // every row arrives from the source
            false,      // the query is a regex, not a fuzzy pattern - no path tuning
            theme,
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
            cwd: std::env::current_dir().ok(),
            running: None,
            pending: None,
            state: State::Idle,
        }
    }

    /// Query and then drain until the walk finishes. The worker is a thread, so a
    /// single `take` would race it. The wait is stepped over with a synthetic clock
    /// rather than slept through.
    fn run(source: &mut Matches, query: &str) -> Vec<Item> {
        let typed = Instant::now();
        source.restart(query, typed);
        source.due(typed + DEBOUNCE);
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
        let typed = Instant::now();
        source.restart("unclosed(", typed);
        source.due(typed + DEBOUNCE);
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
    fn a_finished_search_stops_claiming_to_be_searching() {
        // Otherwise a pattern that matches nothing says "searching…" forever, and
        // the empty box it sits in is exactly the state the status line exists to
        // explain - so the one unexplained case would be the commonest one.
        let t = TempDir::new();
        t.file("a.rs", "nothing of interest\n");
        let mut source = matches(&t.path);
        assert!(run(&mut source, "zzz-not-here").is_empty());
        assert_eq!(source.status().as_deref(), Some("no matches"));
    }

    #[test]
    fn a_finished_search_that_found_something_is_not_shown_as_empty() {
        // "no matches" is only ever painted where the rows would be, so a search
        // that ended *with* rows must leave the state saying nothing contradictory
        // if the list is ever empty for another reason.
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        assert_eq!(run(&mut source, "needle").len(), 1);
        assert_eq!(source.state, State::Done);
        // A new query puts it straight back to searching, not to a stale "no match".
        source.query("other");
        assert_eq!(source.status().as_deref(), Some("searching…"));
    }

    #[test]
    fn a_query_is_not_walked_until_it_has_stood_still() {
        // The whole point: typing `needle` is six queries, and six walks each
        // cancelled by the next is redundant work for the length of a file apiece.
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        let typed = Instant::now();
        source.restart("needle", typed);
        source.due(typed + DEBOUNCE / 2);
        assert!(source.running.is_none(), "still within the wait");
        assert_eq!(
            source.status().as_deref(),
            Some("searching…"),
            "the wait is part of the search, as far as the user is told"
        );
        source.due(typed + DEBOUNCE);
        assert!(source.running.is_some(), "due, so the walk started");
    }

    #[test]
    fn typing_on_restarts_the_wait_rather_than_the_walk() {
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        let typed = Instant::now();
        source.restart("need", typed);
        // The next keystroke lands inside the wait, so what comes due is the whole
        // word and not the prefix - the prefix is never walked at all.
        source.restart("needle", typed + DEBOUNCE / 2);
        source.due(typed + DEBOUNCE);
        assert!(
            source.running.is_none(),
            "the wait restarted with the query"
        );
        source.due(typed + DEBOUNCE / 2 + DEBOUNCE);
        assert!(source.running.is_some());
        assert_eq!(
            source.pending, None,
            "the pattern was consumed, not re-walked"
        );
    }

    #[test]
    fn clearing_the_query_drops_a_walk_that_was_still_pending() {
        // Backspacing to nothing must not leave a walk armed to start 150ms later.
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        let mut source = matches(&t.path);
        let typed = Instant::now();
        source.restart("needle", typed);
        source.restart("", typed + DEBOUNCE / 2);
        source.due(typed + DEBOUNCE * 2);
        assert!(source.running.is_none());
        assert_eq!(source.status(), None);
    }

    #[test]
    fn the_preview_shows_the_match_in_its_surroundings() {
        // A list of path:line rows leaves open the question the pane answers: one
        // line of a match rarely says whether it is the one you meant.
        let t = TempDir::new();
        t.file("a.rs", "line0\nline1\nline2\nline3\nNEEDLE\nline5\nline6\n");
        let item = row(
            &Hit {
                path: t.path.join("a.rs"),
                line: 4,
                column: 0,
                text: "NEEDLE".to_string(),
            },
            None,
        );
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
        let item = row(
            &Hit {
                path: t.path.join("a.rs"),
                line: 0,
                column: 0,
                text: "first".to_string(),
            },
            None,
        );
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
}
