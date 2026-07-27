//! In-buffer search and replace (SPEC §11, §14 M7) - the surfaces, and the
//! frontend's memory of the current query.
//!
//! The frontend half of the search the core owns. `vortex-core::search` turns a
//! pattern into selections; this turns keystrokes into that pattern, and paints
//! what it would find *before* anything crosses the seam.
//!
//! **The preview is frontend-local, on purpose** (SPEC §7.5's commit-only rule).
//! Typing in the find prompt highlights every match on screen and scrolls the
//! viewport to the one Enter would take you to - and none of that is an `Action`.
//! It does not need to be: the frontend holds the buffer's text (the snapshot's rope,
//! SPEC §5) and owns the viewport, so both halves of "show me what I would get" are
//! answerable locally. Only Enter commits, and only then does the caret move. The
//! upshot is that cancelling a search is free - there is nothing to undo, because
//! nothing happened.
//!
//! **The pattern is matched by the core's own engine**
//! ([`vortex_core::search::compile`]), never by one this module builds. A preview
//! that disagreed with the commit about what matches would be worse than no preview.
//!
//! **The frontend remembers the last search, the core does not.** Find-next needs a
//! pattern to repeat, and putting that memory here - rather than as core state -
//! keeps `Action::SelectNextMatch` a complete message and leaves nothing that could
//! drift out of step with what the user is being shown.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;
use vortex_core::{Action, Text};

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;

/// The live query: the pattern, its compiled form, and what a replace would write.
///
/// `regex` is `None` for a pattern that does not compile *yet* - which is most of
/// them, since a user typing `(\w+)` passes through `(`, `(\`, `(\w` on the way. That
/// is why an uncompilable pattern is a quiet absence of highlights here rather than
/// an error: it is the normal state mid-typing, and only a *committed* one is worth
/// complaining about.
pub struct Query {
    pub pattern: String,
    pub replacement: String,
    regex: Option<regex::Regex>,
}

impl Query {
    /// Compile `pattern` through the core's engine, so the preview and the commit
    /// cannot disagree about what matches.
    pub fn new(pattern: String, replacement: String) -> Self {
        let regex = (!pattern.is_empty())
            .then(|| vortex_core::search::compile(&pattern).ok())
            .flatten();
        Self {
            pattern,
            replacement,
            regex,
        }
    }

    /// Every match on `lines`, for painting. Empty when the pattern is empty or does
    /// not compile.
    pub fn matches_in(&self, text: &Text, lines: Range<usize>) -> Vec<Range<usize>> {
        self.regex
            .as_ref()
            .map(|regex| vortex_core::search::matches_in(text, regex, lines))
            .unwrap_or_default()
    }

    /// The match a commit would land on: the first at or after `from`, wrapping.
    pub fn next_from(&self, text: &Text, from: usize) -> Option<Range<usize>> {
        vortex_core::search::next_match(text, self.regex.as_ref()?, from, false)
    }
}

/// What the frontend remembers about searching: the current query and, while a find
/// prompt is open, the match it is previewing.
///
/// Outlives the prompt deliberately. The highlights stay up after Enter so a
/// find-next key has something to walk, and the pattern stays so it has something to
/// repeat - both cleared by Escape, which is the one gesture that means "done
/// searching".
#[derive(Default)]
pub struct SearchState {
    query: Option<Query>,
    /// The match the live preview is showing, in buffer bytes. `Some` only while a
    /// find prompt is open: after the commit the *caret* is on the match, and the
    /// ordinary caret-follow takes over.
    preview: Option<Range<usize>>,
    /// Where the caret was when the prompt opened - what the preview searches from,
    /// so refining a query keeps finding the match nearest where the user was rather
    /// than walking away from it one keystroke at a time.
    origin: usize,
}

impl SearchState {
    /// The current query, if a search is live.
    pub fn query(&self) -> Option<&Query> {
        self.query.as_ref()
    }

    /// The previewed match, for the frame to follow and paint as the current one.
    pub fn preview(&self) -> Option<Range<usize>> {
        self.preview.clone()
    }

    /// Start a search from `origin` - the caret when the prompt opened.
    pub fn begin(&mut self, origin: usize) {
        self.origin = origin;
        self.preview = None;
    }

    /// Take a new pattern from an open prompt, re-deriving the preview against
    /// `text`. Called per keystroke: the match scan is lazy and stops at the first
    /// hit (SPEC §11), so it costs the lines between the origin and the match, not
    /// the file.
    ///
    /// Takes the text rather than the snapshot it came from because that is all it
    /// needs, and `None` (no snapshot yet) then keeps the query without a preview
    /// rather than being a case this has to know about.
    pub fn refresh(&mut self, query: Query, text: Option<&Text>) {
        self.preview = text.and_then(|text| query.next_from(text, self.origin));
        self.query = Some(query);
    }

    /// Stop previewing but keep the query, so the highlights and the repeatable
    /// pattern survive the prompt closing.
    pub fn commit(&mut self) {
        self.preview = None;
    }

    /// Forget the search entirely - Escape, and the end of the highlights.
    pub fn clear(&mut self) {
        self.query = None;
        self.preview = None;
    }

    /// The action a find-next/find-previous key resolves to, or `None` when nothing
    /// has been searched for yet (the key is then a no-op rather than a round trip).
    pub fn repeat(&self, backward: bool) -> Option<Action> {
        let pattern = self.query.as_ref()?.pattern.clone();
        (!pattern.is_empty()).then_some(Action::SelectNextMatch { pattern, backward })
    }
}

/// Which field of a replace prompt the typing goes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Pattern,
    Replacement,
}

/// The find (or find-and-replace) prompt: one or two editable fields on the bottom
/// row, publishing a live query as you type.
///
/// A sibling of [`crate::prompt::Prompt`] rather than a configuration of it: this one
/// emits on *every* keystroke rather than only on commit, and has two fields with a
/// focus between them - neither of which a one-shot value prompt has any business
/// carrying.
pub struct Find {
    pattern: String,
    replacement: String,
    /// Whether the replacement field exists at all. A plain find shows one field; a
    /// replace shows two, with Tab between them.
    replacing: bool,
    focus: Field,
    style: Style,
    finished: bool,
    outbox: Vec<Command>,
}

impl Find {
    /// A find prompt, seeded with `pattern` (the last search, so reopening offers it
    /// again rather than making the user retype it).
    pub fn new(theme: &Theme, pattern: String, replacing: bool) -> Self {
        Self {
            pattern,
            replacement: String::new(),
            replacing,
            focus: Field::Pattern,
            style: theme.palette,
            finished: false,
            outbox: Vec::new(),
        }
    }

    /// The prompt's row: the screen's bottom line, as [`crate::prompt::Prompt`] uses.
    fn row(screen: Rect) -> Option<Rect> {
        (screen.height > 0 && screen.width > 0)
            .then(|| Rect::new(screen.x, screen.bottom() - 1, screen.width, 1))
    }

    /// The label before each field.
    fn prefix(field: Field) -> &'static str {
        match field {
            Field::Pattern => "Find: ",
            Field::Replacement => "  Replace: ",
        }
    }

    /// The whole rendered line: the pattern, and the replacement if there is one.
    ///
    /// **No match count**, deliberately. Counting every match is a scan of the whole
    /// buffer per keystroke, which is the one thing SPEC §10.4 rules out off the
    /// viewport - and the question it would answer ("is this finding anything") is
    /// already answered by the highlights on screen and by the viewport moving to the
    /// match. A commit that finds nothing says so as a toast, from the core.
    fn line(&self) -> String {
        let mut out = format!("{}{}", Self::prefix(Field::Pattern), self.pattern);
        if self.replacing {
            out.push_str(Self::prefix(Field::Replacement));
            out.push_str(&self.replacement);
        }
        out
    }

    /// The cursor's display column: past the prefix and text of the focused field.
    fn caret_column(&self) -> usize {
        let found = Self::prefix(Field::Pattern).width() + self.pattern.width();
        match self.focus {
            Field::Pattern => found,
            Field::Replacement => {
                found + Self::prefix(Field::Replacement).width() + self.replacement.width()
            }
        }
    }

    /// The field the typing goes into.
    fn active(&mut self) -> &mut String {
        match self.focus {
            Field::Pattern => &mut self.pattern,
            Field::Replacement => &mut self.replacement,
        }
    }

    /// Publish the query as it now stands - after every edit, so the highlights and
    /// the previewed match track the typing rather than the commit.
    fn publish(&mut self) {
        self.outbox.push(Command::PreviewSearch {
            pattern: self.pattern.clone(),
            replacement: self.replacement.clone(),
        });
    }
}

impl Layer for Find {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some(row) = Self::row(screen) else {
            return;
        };
        buf.set_style(row, self.style);
        buf.set_stringn(row.x, row.y, self.line(), row.width as usize, self.style);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // A Ctrl/Cmd chord is a keybinding, not input - deferred so the shortcut runs,
        // the same rule `Prompt` and the pickers follow (SPEC §7.5).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER)
        {
            return EventResult::Ignored;
        }
        match key.code {
            // Escape is the one gesture that means "done searching": it takes the
            // highlights down with it, which is why it commits a clear rather than
            // just closing.
            KeyCode::Esc => {
                self.outbox.push(Command::ClearSearch);
                self.finished = true;
            }
            KeyCode::Enter => {
                // A replace commits into the query-replace walk; a plain find just
                // goes to the match. Either way the query survives, so the
                // highlights stay up and find-next has a pattern to repeat.
                self.outbox.push(if self.replacing {
                    Command::StartReplace
                } else {
                    Command::FindNext
                });
                self.finished = true;
            }
            // Tab moves between the two fields of a replace. In a plain find there is
            // nowhere to go, so it is swallowed rather than inserting a tab into the
            // pattern, where it would be a character the user cannot see.
            KeyCode::Tab if self.replacing => {
                self.focus = match self.focus {
                    Field::Pattern => Field::Replacement,
                    Field::Replacement => Field::Pattern,
                };
            }
            KeyCode::Backspace => {
                self.active().pop();
                self.publish();
            }
            KeyCode::Char(c) => {
                self.active().push(c);
                self.publish();
            }
            // Modal: swallow anything else so it never reaches the editor beneath.
            _ => {}
        }
        EventResult::Consumed
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, screen: Rect) -> EventResult {
        let on_row = Self::row(screen)
            .is_some_and(|row| row.contains(Position::new(mouse.column, mouse.row)));
        // Clicking away is "never mind", as it is for a prompt - and like Escape it
        // takes the highlights down, since the search is over either way.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && !on_row {
            self.outbox.push(Command::ClearSearch);
            self.finished = true;
        }
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        let row = Self::row(screen)?;
        let x = (row.x as usize + self.caret_column()).min(row.right().saturating_sub(1) as usize)
            as u16;
        Some(Position::new(x, row.y))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn restyle(&mut self, theme: &Theme) {
        self.style = theme.palette;
    }
}

/// The query-replace walk: sit on a match and ask what to do with it (SPEC §11).
///
/// The shape a terminal can actually offer for "replace one at a time". The
/// alternative - a chord that means replace-and-advance while the find prompt stays
/// open - needs a modifier combination classic terminals cannot report (SPEC §9), so
/// the flow is a question per match instead: `y` replaces and moves on, `n` skips,
/// `a` finishes the rest in one edit, and `q`/Escape stops. It is Vim's `:s///c` and
/// Emacs' `query-replace`, and it needs exactly one key per answer.
///
/// The surface holds no match list. Each answer commits actions and the *core*
/// re-finds from wherever the caret ended up, so a walk cannot be following a set of
/// positions that an edit has since moved - the mistake SPEC §2.1 exists to prevent.
pub struct QueryReplace {
    pattern: String,
    replacement: String,
    style: Style,
    finished: bool,
    outbox: Vec<Command>,
}

impl QueryReplace {
    pub fn new(theme: &Theme, pattern: String, replacement: String) -> Self {
        Self {
            pattern,
            replacement,
            style: theme.palette,
            finished: false,
            outbox: Vec::new(),
        }
    }

    /// The question, naming both halves so the user can see what they agreed to.
    fn question(&self) -> String {
        format!(
            "Replace `{}` with `{}`? (y)es (n)o (a)ll (q)uit ",
            self.pattern, self.replacement
        )
    }

    /// Replace the match under the caret and move to the next.
    fn replace_one(&self) -> Vec<Command> {
        vec![
            Command::Editor(Action::ReplaceMatch {
                pattern: self.pattern.clone(),
                replacement: self.replacement.clone(),
            }),
            Command::Editor(Action::SelectNextMatch {
                pattern: self.pattern.clone(),
                backward: false,
            }),
        ]
    }
}

impl Layer for QueryReplace {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some(row) = Find::row(screen) else {
            return;
        };
        buf.set_style(row, self.style);
        buf.set_stringn(
            row.x,
            row.y,
            self.question(),
            row.width as usize,
            self.style,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER)
        {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let commands = self.replace_one();
                self.outbox.extend(commands);
                // Deliberately still open: the point of a walk is the next question.
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.outbox.push(Command::Editor(Action::SelectNextMatch {
                    pattern: self.pattern.clone(),
                    backward: false,
                }));
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.outbox.push(Command::Editor(Action::ReplaceAllMatches {
                    pattern: self.pattern.clone(),
                    replacement: self.replacement.clone(),
                }));
                self.finished = true;
            }
            // Every other key stops the walk without replacing - `q`, Escape, or a
            // stray one. The destructive answers are exactly `y` and `a`, so a
            // mistyped key can only ever end the walk early.
            _ => {
                self.outbox.push(Command::ClearSearch);
                self.finished = true;
            }
        }
        EventResult::Consumed
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _screen: Rect) -> EventResult {
        // A click is not one of the two destructive answers, wherever it lands: it
        // ends the walk, the same stance `Confirm` takes.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.outbox.push(Command::ClearSearch);
            self.finished = true;
        }
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn restyle(&mut self, theme: &Theme) {
        self.style = theme.palette;
    }
}

#[cfg(test)]
#[path = "buffersearch_tests.rs"]
mod tests;
