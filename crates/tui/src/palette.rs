//! The command palette overlay (SPEC §7.5) - a fuzzy-filtered list of named
//! commands, opened with Ctrl+P.
//!
//! It is a discovery surface over the same [`Command`]s a key can run: type to
//! filter, Up/Down to move, Enter to run the highlighted one, Esc to cancel. Picking
//! emits that command (the §7.5 seam rule - navigation stays frontend-local, only the
//! chosen command leaves the layer), so a palette pick and a bound key run through the
//! identical dispatch path. Fuzzy matching uses `nucleo` (Helix's matcher).
//!
//! The listed set is curated, not every keymap binding: motions and text entry are
//! not things a user picks by name. Filtering is done on this thread against a small
//! static list, which `nucleo-matcher`'s `match_list` is meant for.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Widget};
use unicode_width::UnicodeWidthStr;
use vortex_core::Action;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;

/// One selectable command: a user-facing label and what running it does.
struct Entry {
    label: &'static str,
    command: Command,
}

/// The curated command set the palette lists - the discrete named commands a user
/// picks by name (SPEC §7.5), deliberately excluding motions and text entry.
fn registry() -> Vec<Entry> {
    let e = |label, command| Entry { label, command };
    vec![
        e("Open File…", Command::OpenFilePrompt),
        e("Save File", Command::Editor(Action::Save)),
        e("Undo", Command::Editor(Action::Undo)),
        e("Redo", Command::Editor(Action::Redo)),
        e("Copy", Command::Editor(Action::Copy)),
        e("Cut", Command::Editor(Action::Cut)),
        e("Paste", Command::Editor(Action::Paste)),
        e("Add Cursor Above", Command::Editor(Action::AddCursorAbove)),
        e("Add Cursor Below", Command::Editor(Action::AddCursorBelow)),
        e(
            "Collapse Selections",
            Command::Editor(Action::CollapseSelections),
        ),
        e("Quit", Command::Editor(Action::Quit)),
    ]
}

/// A registry entry paired with its index, so `nucleo`'s `match_list` (which needs
/// `AsRef<str>` haystacks) hands the index straight back after ranking - no lookup.
struct Ranked {
    idx: usize,
    label: &'static str,
}

impl AsRef<str> for Ranked {
    fn as_ref(&self) -> &str {
        self.label
    }
}

/// The palette: a filter query, the ranked subset of commands matching it, and the
/// highlighted row.
pub struct Palette {
    entries: Vec<Entry>,
    /// The current filter text (appended/backspaced; a filter, not a full editor).
    query: String,
    /// Indices into `entries`, ranked by `query` (all, in order, when empty).
    filtered: Vec<usize>,
    /// Row into `filtered` that is highlighted.
    selected: usize,
    matcher: Matcher,
    style: Style,
    selected_style: Style,
    finished: bool,
    committed: Option<Command>,
}

impl Palette {
    fn new(style: Style, selected_style: Style) -> Self {
        let entries = registry();
        let filtered = (0..entries.len()).collect();
        Self {
            entries,
            query: String::new(),
            filtered,
            selected: 0,
            matcher: Matcher::new(Config::DEFAULT),
            style,
            selected_style,
            finished: false,
            committed: None,
        }
    }

    /// Recompute the ranked subset for the current query. An empty query lists every
    /// command in registry order; otherwise `nucleo` ranks by fuzzy score.
    fn refilter(&mut self) {
        self.filtered = if self.query.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            let ranked = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart)
                .match_list(
                    self.entries.iter().enumerate().map(|(idx, e)| Ranked {
                        idx,
                        label: e.label,
                    }),
                    &mut self.matcher,
                );
            ranked.into_iter().map(|(r, _score)| r.idx).collect()
        };
        // Keep the highlight in range as the list shrinks.
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The centered box the palette occupies, clamped to the screen.
    fn area(screen: Rect) -> Rect {
        let w = screen.width.min(50);
        let h = screen.height.min(14);
        let x = screen.x + (screen.width - w) / 2;
        let y = screen.y + (screen.height - h) / 2;
        Rect::new(x, y, w, h)
    }
}

impl Layer for Palette {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        // Below this the box has no usable interior; the editor is unobstructed.
        if screen.width < 10 || screen.height < 4 {
            return;
        }
        let area = Self::area(screen);
        Clear.render(area, buf);
        let block = Block::bordered().title(" Commands ").style(self.style);
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // Query row at the top of the interior.
        let query_line = format!("> {}", self.query);
        buf.set_stringn(
            inner.x,
            inner.y,
            &query_line,
            inner.width as usize,
            self.style,
        );
        // The list fills the rows beneath it, scrolled to keep the highlight visible.
        let list_h = inner.height.saturating_sub(1) as usize;
        if list_h == 0 {
            return;
        }
        let scroll = self.selected.saturating_sub(list_h - 1);
        for (row, &idx) in self.filtered.iter().enumerate().skip(scroll).take(list_h) {
            let y = inner.y + 1 + (row - scroll) as u16;
            let style = if row == self.selected {
                self.selected_style
            } else {
                self.style
            };
            let rect = Rect::new(inner.x, y, inner.width, 1);
            buf.set_style(rect, style);
            let line = format!("  {}", self.entries[idx].label);
            buf.set_stringn(inner.x, y, &line, inner.width as usize, style);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Esc => self.finished = true,
            KeyCode::Enter => {
                if let Some(&idx) = self.filtered.get(self.selected) {
                    self.committed = Some(self.entries[idx].command.clone());
                }
                self.finished = true;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                let last = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
                self.selected = 0;
            }
            // Typing filters. A Ctrl/Cmd-modified char is a chord, not text (same rule
            // as the keymap and prompt), so it is swallowed rather than added.
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SUPER) =>
            {
                self.query.push(c);
                self.refilter();
                self.selected = 0;
            }
            // Modal: swallow anything else so it never reaches the editor beneath.
            _ => {}
        }
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        self.committed.take().into_iter().collect()
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        if screen.width < 10 || screen.height < 4 {
            return None;
        }
        let inner = Block::bordered().inner(Self::area(screen));
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        // Caret in the query row, after the "> " prompt plus the typed text.
        let col = 2 + self.query.width();
        let x = (inner.x as usize + col).min(inner.right().saturating_sub(1) as usize) as u16;
        Some(Position::new(x, inner.y))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Open the command palette, styled from the theme.
pub fn open(theme: &Theme) -> Box<dyn Layer> {
    Box::new(Palette::new(theme.palette, theme.palette_selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::new(Style::default(), Style::default())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Palette, s: &str) {
        for c in s.chars() {
            p.handle_key(key(c));
        }
    }

    /// The label of the currently highlighted entry, for assertions.
    fn selected_label(p: &Palette) -> &str {
        p.entries[p.filtered[p.selected]].label
    }

    #[test]
    fn starts_listing_every_command() {
        let p = palette();
        assert_eq!(p.filtered.len(), p.entries.len());
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn typing_fuzzy_filters_the_list() {
        let mut p = palette();
        type_str(&mut p, "save");
        assert!(!p.filtered.is_empty(), "‘save’ should match something");
        assert_eq!(selected_label(&p), "Save File");
        // A non-matching query empties the list.
        let mut p = palette();
        type_str(&mut p, "zzzzzq");
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn down_and_up_move_the_selection_clamped() {
        let mut p = palette();
        assert_eq!(p.selected, 0);
        p.handle_key(press(KeyCode::Up)); // already at top - clamps
        assert_eq!(p.selected, 0);
        p.handle_key(press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        // Jump past the end - clamps to the last row.
        for _ in 0..100 {
            p.handle_key(press(KeyCode::Down));
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn enter_commits_the_highlighted_command_and_finishes() {
        let mut p = palette();
        // First entry is "Open File…" -> OpenFilePrompt.
        assert_eq!(selected_label(&p), "Open File…");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert_eq!(p.take_commands(), vec![Command::OpenFilePrompt]);
    }

    #[test]
    fn filtered_enter_runs_the_matched_command() {
        let mut p = palette();
        type_str(&mut p, "quit");
        p.handle_key(press(KeyCode::Enter));
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Quit)]);
    }

    #[test]
    fn esc_cancels_with_no_command() {
        let mut p = palette();
        p.handle_key(press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn enter_on_an_empty_result_commits_nothing() {
        let mut p = palette();
        type_str(&mut p, "zzzzzq");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        // "quit" is unique (only entry with a 'q'); deleting it back to empty
        // restores the full list.
        let mut p = palette();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        for _ in 0..4 {
            p.handle_key(press(KeyCode::Backspace));
        }
        assert!(p.query.is_empty());
        assert_eq!(p.filtered.len(), p.entries.len());
    }

    #[test]
    fn renders_a_centered_box_with_query_and_entries() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = palette();
        type_str(&mut p, "sa");
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Commands"), "border title present");
        assert!(text.contains("> sa"), "query row present");
        assert!(text.contains("Save File"), "a matching entry is listed");
        // The caret sits in the query row, after "> sa" (4 columns in).
        let inner = Block::bordered().inner(Palette::area(Rect::new(0, 0, 40, 16)));
        assert_eq!(
            p.cursor(Rect::new(0, 0, 40, 16)),
            Some(Position::new(inner.x + 4, inner.y))
        );
    }
}
