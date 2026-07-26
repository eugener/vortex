//! Cross-file search (SPEC §7.5, §14 M7) - the worker behind the global-search
//! picker ([`crate::globalsearch`]).
//!
//! **Frontend-owned, on a background thread.** A grep is filesystem work, and the
//! core is a single-owner actor whose thread every keystroke goes through: a walk of
//! a project on it would stall editing. So this is shaped like the other producers
//! the frontend spawns (the LSP client, the highlighter, the file watcher) - it runs
//! beside the editor and sends results in - except that nothing it produces crosses
//! the seam. A hit is a *frontend* fact until it is picked, and only the pick becomes
//! an `Action` (§7.5's commit-only rule).
//!
//! **It searches the files, not the buffers.** An unsaved edit is not visible to it:
//! buffer contents other than the active document's do not cross the seam, and
//! asking for them would put the search back on the actor thread it exists to stay
//! off. So a hit you have typed but not saved will not appear, and one you have
//! deleted but not saved still will. The alternative is a new seam message per
//! keystroke of a query; this is the honest trade and it is the one grep makes too.
//!
//! Each query gets its own thread and its own channel. A new query drops the old
//! [`Search`], which flags the old thread to stop and closes its channel, so a stale
//! walk cannot deliver results into a list that has moved on.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

use regex::RegexBuilder;

/// Cap on a file's size to search. Above this it is a database, a log, or a build
/// artifact - the things a source grep is not for, and the ones that would spend the
/// whole walk's budget on one entry.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Cap on hits delivered for one query. A pattern like `.` matches every line in the
/// project; the list is a picker, so past a point more rows are not more useful, and
/// the walk stops rather than filling memory with them.
const MAX_HITS: usize = 1000;

/// How many hits may sit in the channel before the worker blocks. Bounded so a
/// stalled UI applies back-pressure to the walk instead of queueing it all up
/// (SPEC §6, the same bargain the core's channels make).
const CHANNEL_DEPTH: usize = 64;

/// Cap on a matched line's length as reported, in characters - the unit the row is
/// clipped in, since the picker paints cells and not bytes. A minified file matches
/// on a line megabytes long; the picker shows one row of it, and carrying the rest to
/// throw away costs more than the match did.
const MAX_LINE_CHARS: usize = 500;

/// One match: the file, where in it, and the line it was on.
///
/// `line` and `column` are **0-based**, the coordinates
/// [`vortex_core::Action::PlaceCursorAt`] takes - the picker passes them straight
/// through rather than converting at the last moment. `column` is a byte offset
/// within the line, on a character boundary because a regex match starts on one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    /// The matched line's text, trimmed of leading whitespace and capped at
    /// [`MAX_LINE_CHARS`] characters. What the picker shows as the row.
    pub text: String,
}

/// A running search. Dropping it cancels the walk.
pub struct Search {
    hits: Receiver<Hit>,
    cancel: Arc<AtomicBool>,
    /// Set once the worker has dropped its sender, which it does by returning -
    /// whether it finished the walk, hit a cap, or saw the cancel flag.
    finished: bool,
}

impl Search {
    /// Every hit that has arrived since the last call, without blocking.
    ///
    /// Drains rather than taking one at a time so a frame's worth of results lands
    /// in one repaint, and returns an empty vec once the walk is over - which
    /// [`Self::is_finished`] is there to tell apart from a walk that is merely
    /// between files.
    pub fn drain(&mut self) -> Vec<Hit> {
        let mut out = Vec::new();
        loop {
            match self.hits.try_recv() {
                Ok(hit) => out.push(hit),
                Err(TryRecvError::Empty) => return out,
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    return out;
                }
            }
        }
    }

    /// Whether the walk has ended. Only true after a [`Self::drain`] has seen the
    /// channel close, so a caller that never drains never claims a search is done.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        // The thread checks this between files, so it stops within one file rather
        // than running the whole tree for a query nobody is showing any more.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start searching `root` for `pattern`, case-insensitively unless the pattern has
/// an uppercase letter in it (smart case - typing lowercase means "I do not care",
/// typing a capital means you did).
///
/// # Errors
/// Returns the `regex` compile error for a pattern that is not one, so the picker
/// can show it on the row where results would be rather than searching for nothing.
pub fn spawn(root: &Path, pattern: &str) -> Result<Search, regex::Error> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()?;
    let (tx, hits) = sync_channel(CHANNEL_DEPTH);
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancel);
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let mut sent = 0usize;
        // Single-threaded walk. `ignore` offers a parallel one, but the results
        // would then arrive in an order that changes run to run, and a picker list
        // that reshuffles under the highlight is worse than one that fills slower.
        //
        // `require_git(false)` because the default only reads a `.gitignore` inside
        // a git repository, and a directory that says what is generated is saying it
        // whether or not it has been committed yet.
        let walk = ignore::WalkBuilder::new(&root).require_git(false).build();
        for entry in walk.flatten() {
            if stop.load(Ordering::Relaxed) || sent >= MAX_HITS {
                return;
            }
            if entry.file_type().is_none_or(|t| !t.is_file()) {
                continue;
            }
            for hit in search_file(entry.path(), &regex, MAX_HITS - sent) {
                // A closed channel is the picker having gone away mid-file.
                if tx.send(hit).is_err() {
                    return;
                }
                sent += 1;
            }
        }
    });
    Ok(Search {
        hits,
        cancel,
        finished: false,
    })
}

/// Every match in one file, up to `limit`.
///
/// Reads the whole file (bounded by [`MAX_FILE_BYTES`]) and decodes it with the
/// core's own loader, so a windows-1252 source file is searched as the text the
/// editor would open, not as bytes. Unreadable files are skipped rather than
/// reported: a walk crosses plenty of them and a permission error per row would
/// bury the matches (SPEC §8's defensive I/O, the file picker's walk does the same).
fn search_file(path: &Path, regex: &regex::Regex, limit: usize) -> Vec<Hit> {
    let Some(bytes) = read_capped(path) else {
        return Vec::new();
    };
    if vortex_core::file::is_binary(&bytes) {
        return Vec::new();
    }
    let text = vortex_core::file::load(&bytes).text;
    text.lines()
        .enumerate()
        .filter_map(|(line, content)| {
            let found = regex.find(content)?;
            Some(Hit {
                path: path.to_path_buf(),
                line,
                column: found.start(),
                text: trim_row(content),
            })
        })
        .take(limit)
        .collect()
}

/// A file's bytes, or `None` if it cannot be read or is too big to be source.
fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// A matched line as a picker row: indentation dropped so deeply nested code does
/// not show as a column of blanks, and capped so one long line cannot be the row.
fn trim_row(line: &str) -> String {
    let line = line.trim_start();
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => line[..end].to_string(),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Run a search to completion. The worker is a thread, so a test that asserted
    /// on one `drain` would race it; this drains until the channel closes.
    fn collect(root: &Path, pattern: &str) -> Vec<Hit> {
        let mut search = spawn(root, pattern).expect("a valid pattern");
        let mut hits = Vec::new();
        // The worker holds the sender; when it returns, `recv` errors and we are done.
        while let Ok(hit) = search.hits.recv() {
            hits.push(hit);
        }
        hits.append(&mut search.drain());
        hits.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        hits
    }

    #[test]
    fn finds_a_match_with_its_position_and_line() {
        let t = TempDir::new();
        t.file("a.rs", "fn main() {\n    let needle = 1;\n}\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, t.path.join("a.rs"));
        assert_eq!(hits[0].line, 1, "0-based, as PlaceCursorAt takes it");
        assert_eq!(hits[0].column, 8, "byte offset into the line, not the file");
        assert_eq!(hits[0].text, "let needle = 1;", "indentation dropped");
    }

    #[test]
    fn searches_every_file_in_the_tree() {
        let t = TempDir::new();
        t.file("a.rs", "needle\n");
        t.file("deep/nested/b.rs", "also needle here\n");
        t.file("c.rs", "nothing\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, t.path.join("a.rs"));
        assert_eq!(hits[1].path, t.path.join("deep/nested/b.rs"));
    }

    #[test]
    fn one_hit_per_line_even_when_a_line_matches_twice() {
        // The row is "this line, jump here"; a second column on the same line would
        // be a second row that looks identical and lands two characters away.
        let t = TempDir::new();
        t.file("a.rs", "needle needle needle\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, 0, "the first match on the line");
    }

    #[test]
    fn the_pattern_is_a_regex() {
        let t = TempDir::new();
        t.file("a.rs", "fn alpha() {}\nfn beta() {}\nlet gamma = 1;\n");
        let hits = collect(&t.path, r"^fn \w+\(");
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].line, hits[1].line), (0, 1));
    }

    #[test]
    fn a_pattern_that_is_not_a_regex_is_reported_rather_than_searched() {
        let t = TempDir::new();
        t.file("a.rs", "x\n");
        assert!(spawn(&t.path, "unclosed(").is_err());
    }

    #[test]
    fn case_is_smart() {
        // Lowercase means "I do not care"; a capital means you did.
        let t = TempDir::new();
        t.file("a.rs", "Needle\nneedle\n");
        assert_eq!(
            collect(&t.path, "needle").len(),
            2,
            "lowercase matches both"
        );
        let upper = collect(&t.path, "Needle");
        assert_eq!(upper.len(), 1, "a capital is meant");
        assert_eq!(upper[0].line, 0);
    }

    #[test]
    fn gitignored_and_hidden_files_are_not_searched() {
        // The reason `ignore` is the walker: a project's own .gitignore decides,
        // rather than the file picker's hardcoded target/node_modules list.
        let t = TempDir::new();
        t.file(".gitignore", "build/\n");
        t.file("build/generated.rs", "needle\n");
        t.file(".secret.rs", "needle\n");
        t.file("src/real.rs", "needle\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, t.path.join("src/real.rs"));
    }

    #[test]
    fn a_binary_file_is_skipped_rather_than_matched_as_mojibake() {
        let t = TempDir::new();
        t.file("blob.png", "\u{0}\u{1}needle\n");
        t.file("a.rs", "needle\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, t.path.join("a.rs"));
    }

    #[test]
    fn a_file_is_searched_in_the_encoding_it_would_open_in() {
        // windows-1252, the encoding the core keeps a nameless file in: searching
        // the raw bytes instead would never match what the editor shows.
        let t = TempDir::new();
        std::fs::write(t.path.join("latin.txt"), b"le caf\xe9 est ouvert\n").unwrap();
        let hits = collect(&t.path, "café");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "le café est ouvert");
    }

    #[test]
    fn a_file_too_big_to_be_source_is_skipped() {
        let t = TempDir::new();
        let huge = "x".repeat(MAX_FILE_BYTES as usize + 1) + "\nneedle\n";
        t.file("huge.log", &huge);
        t.file("a.rs", "needle\n");
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1, "the oversized file was searched anyway");
        assert_eq!(hits[0].path, t.path.join("a.rs"));
    }

    #[test]
    fn a_very_long_matched_line_is_capped_as_a_row() {
        let t = TempDir::new();
        t.file("min.js", &format!("needle{}\n", "x".repeat(5000)));
        let hits = collect(&t.path, "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn the_hit_count_is_capped() {
        // `.` matches every line in the project; the walk stops rather than filling
        // memory with rows nobody will scroll to.
        let t = TempDir::new();
        t.file("a.rs", &"line\n".repeat(MAX_HITS + 500));
        assert_eq!(collect(&t.path, "line").len(), MAX_HITS);
    }

    #[test]
    fn dropping_the_search_stops_the_walk() {
        // A new query must not have the old one still delivering into it. Proven by
        // the channel closing: the worker returns, which drops its sender.
        let t = TempDir::new();
        for n in 0..200 {
            t.file(&format!("f{n}.rs"), "needle\n");
        }
        let search = spawn(&t.path, "needle").expect("a valid pattern");
        let hits = search.hits.recv(); // let the walk get going
        assert!(hits.is_ok());
        search.cancel.store(true, Ordering::Relaxed);
        // Whatever was already in the channel still drains; the point is that it
        // ends, rather than running all 200 files into a list nobody is showing.
        let mut drained = 0;
        while search.hits.recv().is_ok() {
            drained += 1;
            assert!(drained < 200, "the walk kept going after cancellation");
        }
    }

    #[test]
    fn an_empty_tree_finds_nothing_and_finishes() {
        let t = TempDir::new();
        assert!(collect(&t.path, "needle").is_empty());
    }
}
