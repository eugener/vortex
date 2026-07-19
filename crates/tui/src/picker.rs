//! A generic fuzzy picker overlay (SPEC §7.5) - a filter query over a list of named
//! items, each carrying the [`Command`] to run when it is chosen.
//!
//! This is the shared machinery behind the command palette ([`crate::palette`]) and
//! the file picker ([`crate::filepicker`]): they differ only in what fills the list
//! and what a pick runs, not in how you filter, move, and select. Type to filter
//! (via `nucleo`, Helix's matcher), Up/Down to move, Enter to run the highlighted
//! item, Esc to cancel. Picking emits that item's command - the §7.5 seam rule, so a
//! pick and a bound key run through the identical dispatch path.
//!
//! Filtering runs on this thread; `nucleo-matcher`'s `match_list` is meant for the
//! small-to-moderate lists here (a command set, a capped file walk). A very large
//! corpus would want the async high-level `nucleo` crate instead - deferred.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};

/// One selectable row: a user-facing label and what running it does.
pub struct Item {
    pub label: String,
    pub command: Command,
}

/// A label paired with its index, so `nucleo`'s `match_list` (which needs
/// `AsRef<str>` haystacks) hands the index straight back after ranking - no lookup.
struct Ranked<'a> {
    idx: usize,
    label: &'a str,
}

impl AsRef<str> for Ranked<'_> {
    fn as_ref(&self) -> &str {
        self.label
    }
}

/// A fuzzy picker: a titled box with a filter query, the ranked subset of items
/// matching it, and the highlighted row.
pub struct Picker {
    title: String,
    items: Vec<Item>,
    /// The current filter text (appended/backspaced; a filter, not a full editor).
    query: String,
    /// Indices into `items`, ranked by `query` (all, in order, when empty).
    filtered: Vec<usize>,
    /// Row into `filtered` that is highlighted.
    selected: usize,
    matcher: Matcher,
    style: Style,
    selected_style: Style,
    finished: bool,
    committed: Option<Command>,
}

impl Picker {
    /// A picker titled `title` over `items`. `match_paths` tunes the matcher for
    /// path-shaped haystacks (a file picker) versus plain labels (a command palette).
    pub fn new(
        title: impl Into<String>,
        items: Vec<Item>,
        match_paths: bool,
        style: Style,
        selected_style: Style,
    ) -> Self {
        let config = if match_paths {
            Config::DEFAULT.match_paths()
        } else {
            Config::DEFAULT
        };
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            query: String::new(),
            filtered,
            selected: 0,
            matcher: Matcher::new(config),
            style,
            selected_style,
            finished: false,
            committed: None,
        }
    }

    /// Recompute the ranked subset for the current query. An empty query lists every
    /// item in its original order; otherwise `nucleo` ranks by fuzzy score.
    fn refilter(&mut self) {
        self.filtered = if self.query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let ranked = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart)
                .match_list(
                    self.items.iter().enumerate().map(|(idx, item)| Ranked {
                        idx,
                        label: &item.label,
                    }),
                    &mut self.matcher,
                );
            ranked.into_iter().map(|(r, _score)| r.idx).collect()
        };
        // Keep the highlight in range as the list shrinks.
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The centered box the picker occupies, clamped to the screen.
    fn area(screen: Rect) -> Rect {
        let w = screen.width.min(60);
        let h = screen.height.min(18);
        let x = screen.x + (screen.width - w) / 2;
        let y = screen.y + (screen.height - h) / 2;
        Rect::new(x, y, w, h)
    }
}

impl Layer for Picker {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        // Below this the box has no usable interior; the editor is unobstructed.
        if screen.width < 10 || screen.height < 4 {
            return;
        }
        let area = Self::area(screen);
        Clear.render(area, buf);
        let block = Block::bordered()
            .title(format!(" {} ", self.title))
            .style(self.style);
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
            let line = format!("  {}", self.items[idx].label);
            buf.set_stringn(inner.x, y, &line, inner.width as usize, style);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            KeyCode::Esc => self.finished = true,
            KeyCode::Enter => {
                if let Some(&idx) = self.filtered.get(self.selected) {
                    self.committed = Some(self.items[idx].command.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_core::Action;

    fn items() -> Vec<Item> {
        [
            ("Save File", Command::Editor(Action::Save)),
            ("Open File", Command::OpenFilePrompt),
            ("Quit", Command::Editor(Action::Quit)),
            ("Copy", Command::Editor(Action::Copy)),
        ]
        .into_iter()
        .map(|(label, command)| Item {
            label: label.to_string(),
            command,
        })
        .collect()
    }

    fn picker() -> Picker {
        Picker::new("Test", items(), false, Style::default(), Style::default())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, s: &str) {
        for c in s.chars() {
            p.handle_key(key(c));
        }
    }

    fn selected_label(p: &Picker) -> &str {
        &p.items[p.filtered[p.selected]].label
    }

    #[test]
    fn starts_listing_every_item() {
        let p = picker();
        assert_eq!(p.filtered.len(), p.items.len());
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn typing_fuzzy_filters_and_ranks() {
        let mut p = picker();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(selected_label(&p), "Quit");
        // A non-matching query empties the list.
        let mut p = picker();
        type_str(&mut p, "zzzq");
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn down_and_up_move_the_selection_clamped() {
        let mut p = picker();
        p.handle_key(press(KeyCode::Up)); // at top - clamps
        assert_eq!(p.selected, 0);
        p.handle_key(press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        for _ in 0..100 {
            p.handle_key(press(KeyCode::Down));
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn enter_commits_the_highlighted_command_and_finishes() {
        let mut p = picker();
        type_str(&mut p, "quit");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Quit)]);
    }

    #[test]
    fn esc_cancels_with_no_command() {
        let mut p = picker();
        p.handle_key(press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn enter_on_an_empty_result_commits_nothing() {
        let mut p = picker();
        type_str(&mut p, "zzzq");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut p = picker();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        for _ in 0..4 {
            p.handle_key(press(KeyCode::Backspace));
        }
        assert!(p.query.is_empty());
        assert_eq!(p.filtered.len(), p.items.len());
    }

    #[test]
    fn renders_a_centered_titled_box_with_query_and_items() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = picker();
        type_str(&mut p, "sa");
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Test"), "border title present");
        assert!(text.contains("> sa"), "query row present");
        assert!(text.contains("Save File"), "a matching item is listed");
        let inner = Block::bordered().inner(Picker::area(Rect::new(0, 0, 40, 16)));
        assert_eq!(
            p.cursor(Rect::new(0, 0, 40, 16)),
            Some(Position::new(inner.x + 4, inner.y))
        );
    }
}
