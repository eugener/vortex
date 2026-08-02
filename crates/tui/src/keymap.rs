//! Key -> `Action` translation, table-driven so it can be user-configured (SPEC
//! §1, §2.2, §10.5, §12.2).
//!
//! Key->intent mapping is **frontend-owned**: the core only ever sees intent
//! (`Action`), never keystrokes. A future GUI maps its own keys to the same actions.
//!
//! The map is **data, not code**: a [`Keymap`] is a set of `(context, chord ->
//! command)` bindings, and [`command_for_key`] is a pure lookup over it. Both sides of
//! a binding parse from strings ([`Chord::parse`], [`Command::parse`]), and the
//! built-in [`Keymap::default`] is **a file** - `keys.toml`, compiled in with
//! `include_str!` beside the themes and read through [`Keymap::extend_from_table`],
//! the same and only loader a user's `[keys]` table goes through. There is no Rust
//! copy of the defaults to drift from it, and the file is the reference a user copies
//! a row out of. Everything is a pure function of a key event, so it stays
//! unit-testable without a terminal (SPEC §13).
//!
//! **One vocabulary, one table.** [`Command`] names everything a key can be bound to,
//! whether it becomes a core `Action` (`save`, `move_left`) or opens a frontend
//! overlay (`open_palette`, `open_file_picker`) - so overlay triggers are as
//! configurable as edits, and `from_pairs` alone is enough to build a complete
//! keymap. It is also the identity the palette lists and looks shortcuts up by
//! ([`Keymap::shortcut_for`]), so a command's name, its binding, and its palette row
//! cannot drift apart. `Command` carries no runtime data: the typed character and
//! the viewport page size are injected by [`Command::resolve`] at press time.
//!
//! Typing a printable character is a **fallback**, not a binding: an unbound char key
//! with no Ctrl inserts itself, so the map never has to enumerate every letter.
//! Bindings match the **full chord** (modifiers included), so `right` and `shift+right`
//! are distinct entries - `extend` is baked into the command, not derived at runtime.

use std::collections::HashMap;
use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use vortex_core::{Action, Motion};

use crate::command::Command as FrontendCommand;

/// A key identity: a key code plus the modifier state. This is the left side of a
/// binding and the lookup key. Parsed from a string like `"ctrl+s"`, `"cmd+z"`, or
/// `"shift+right"` (see [`Chord::parse`]) so a config file can name it.
///
/// `cmd` is the platform command key - Cmd on macOS, the Super/Win key elsewhere -
/// which crossterm reports as [`KeyModifiers::SUPER`]. It is only delivered by
/// terminals that honor the Kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES`
/// (negotiated at startup); classic terminals intercept Cmd, so a `cmd+` binding is
/// simply never matched there rather than misfiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Chord {
    code: KeyCode,
    ctrl: bool,
    shift: bool,
    alt: bool,
    cmd: bool,
}

impl Chord {
    /// The chord an incoming key event represents (only Ctrl/Shift/Alt/Cmd are
    /// read; other modifier bits are ignored so lookup is stable across terminals).
    ///
    /// A letter is folded to lower case and **its case becomes the shift**, rather
    /// than the shift being read off the modifier bit alone. Terminals disagree about
    /// which of the two they send: with the Kitty protocol negotiated a shifted `y`
    /// arrives as `Y` *and* `SHIFT`, and a classic terminal sends `Y` with no modifier
    /// at all. Deriving it from the case makes `"shift+y"` one row that matches both,
    /// and makes `"ctrl+shift+f"` fire whether `f` or `F` is reported.
    ///
    /// It also makes the upper-case form *expressible*: [`Chord::parse`] lowercases
    /// every token, so without this there is no way to write a chord for a capital
    /// letter. Caps Lock is indistinguishable from Shift here, but that is the
    /// terminal's doing rather than this function's.
    ///
    /// Text entry is unaffected: [`text_key`] reads `key.code` rather than the chord,
    /// so an upper-case letter still types itself.
    fn from_event(key: &KeyEvent) -> Self {
        let (code, shifted_letter) = match key.code {
            KeyCode::Char(c) => (
                KeyCode::Char(c.to_ascii_lowercase()),
                c.is_ascii_uppercase(),
            ),
            other => (other, false),
        };
        Self {
            code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            shift: shifted_letter || key.modifiers.contains(KeyModifiers::SHIFT),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            cmd: key.modifiers.contains(KeyModifiers::SUPER),
        }
    }

    /// Parse a chord string such as `"ctrl+shift+left"`, `"cmd+z"`, `"s"`, or
    /// `"pageup"`. Modifier tokens (`ctrl`/`control`, `shift`, `alt`/`opt`,
    /// `cmd`/`super`/`win`, `mod`) may appear in any order before the key; matching is
    /// case-insensitive. Returns `None` if the key token is unknown. (A literal `+`
    /// key is not yet expressible - a known limit.)
    ///
    /// `mod` is the **platform command modifier** - Cmd on macOS, Ctrl elsewhere -
    /// resolved here, at parse time, so `"mod+c" = "copy"` is one row that means the
    /// right thing on both and no table needs a `cfg!` twin. Everything downstream
    /// sees an ordinary resolved chord, which is what lets [`Chord::display`] name the
    /// key the user actually presses.
    fn parse(spec: &str) -> Option<Self> {
        let mut chord = Chord {
            code: KeyCode::Null,
            ctrl: false,
            shift: false,
            alt: false,
            cmd: false,
        };
        let mut have_key = false;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" | "opt" | "option" => chord.alt = true,
                "cmd" | "command" | "super" | "win" => chord.cmd = true,
                "mod" if cfg!(target_os = "macos") => chord.cmd = true,
                "mod" => chord.ctrl = true,
                key => {
                    chord.code = parse_key_code(key)?;
                    have_key = true;
                }
            }
        }
        have_key.then_some(chord)
    }

    /// A human-readable rendering of the chord for display (e.g. `"Ctrl+S"`,
    /// `"Ctrl+Alt+Up"`), used by the palette to show a command's shortcut. Modifiers
    /// are listed in a stable order; not guaranteed to round-trip through [`parse`]
    /// (display casing differs), but that is not required.
    fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        if self.cmd {
            out.push_str("Cmd+");
        }
        out.push_str(&key_display(self.code));
        out
    }
}

/// A [`KeyCode`] rendered for display (the loose inverse of [`parse_key_code`]).
fn key_display(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

/// A key-code token (already lowercased) to its [`KeyCode`]. A single character maps
/// to `Char`; named keys cover the non-text keys the editor binds.
fn parse_key_code(token: &str) -> Option<KeyCode> {
    Some(match token {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" => KeyCode::PageUp,
        "pagedown" | "page_down" => KeyCode::PageDown,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        // Function keys. `F3` is the conventional find-next, which is what brought
        // them in; the whole range is parsed rather than the one key, so a config can
        // bind any of them without another edit here.
        f if f.starts_with('f') && f.len() > 1 => KeyCode::F(f[1..].parse().ok()?),
        one if one.chars().count() == 1 => KeyCode::Char(one.chars().next()?),
        _ => return None,
    })
}

/// A scope a binding lives in: the editor itself, the platform, or one open surface
/// (SPEC §10.5). At any instant an ordered list of contexts is active - `editor`
/// always, the platform, then one per open overlay bottom-to-top - and a chord is
/// resolved by walking that list from the top down, first match winning.
///
/// A **name**, not a predicate language. Zed, VS Code and Sublime all need an
/// expression grammar because many regions can hold focus at once; this focus chain
/// *is* the compositor's own stack and is two deep, so a stack of names is the same
/// power for none of the machinery. The trigger that would overturn that - the first
/// binding whose condition is not "which surface is on top" - is recorded in §10.5.
///
/// A **closed set**: an unknown name in a config file is an error, not a silently
/// ignored table (SPEC §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    /// The buffer, and the `[keys]` table itself. Always active, always lowest.
    Editor,
    Macos,
    Linux,
    Windows,
    /// Every picker - file, buffer, theme, encoding, line-ending, palette, global
    /// search. One context for all seven, because nothing today would bind them
    /// differently. *Trigger:* the first binding that should differ between two.
    Picker,
    /// The one-line value prompt (save-as).
    Prompt,
    /// The find / replace prompt.
    Find,
    /// A yes/no confirmation (close guard, external-change reload, overwrite).
    Confirm,
    /// The query-replace walk.
    Replace,
}

impl Context {
    /// The platform context for this build. Fixed at compile time, since the
    /// platform cannot change while the editor runs - which is what lets the whole
    /// `cfg!` split live here rather than being spread over the tables.
    pub const PLATFORM: Context = if cfg!(target_os = "macos") {
        Context::Macos
    } else if cfg!(target_os = "windows") {
        Context::Windows
    } else {
        Context::Linux
    };

    /// The contexts active when no overlay is open, **lowest first**: the platform
    /// over the editor. Lookup walks it from the top down.
    const EDITOR_STACK: [Context; 2] = [Context::Editor, Context::PLATFORM];

    /// Parse a context name - the name of a subtable under `[keys]`.
    fn parse(name: &str) -> Option<Context> {
        Some(match name {
            "editor" => Context::Editor,
            "macos" => Context::Macos,
            "linux" => Context::Linux,
            "windows" => Context::Windows,
            "picker" => Context::Picker,
            "prompt" => Context::Prompt,
            "find" => Context::Find,
            "confirm" => Context::Confirm,
            "replace" => Context::Replace,
            _ => return None,
        })
    }

    /// The name, for the error messages that quote it back.
    fn name(self) -> &'static str {
        match self {
            Context::Editor => "editor",
            Context::Macos => "macos",
            Context::Linux => "linux",
            Context::Windows => "windows",
            Context::Picker => "picker",
            Context::Prompt => "prompt",
            Context::Find => "find",
            Context::Confirm => "confirm",
            Context::Replace => "replace",
        }
    }

    /// The scope a binding written here is checked against. A platform context sits
    /// in the *editor's* own stack rather than over a surface, so it holds exactly
    /// what the editor holds and answers to the editor's scope.
    fn scope(self) -> Context {
        match self {
            Context::Macos | Context::Linux | Context::Windows => Context::Editor,
            other => other,
        }
    }
}

/// A bindable command: the intent side of a binding, carrying no runtime data.
///
/// This is the single command vocabulary - the stable identifiers a config file
/// binds to (`save`, `move_left`, `select_page_down`, `open_palette`), the identity
/// the palette lists, and the key [`Keymap::shortcut_for`] matches on. Both kinds of
/// outcome live here: most variants become a core [`Action`], while the overlay
/// triggers stay frontend-local. Keeping them in one enum is what lets a user config
/// rebind an overlay trigger and what makes the reverse lookup an exact match rather
/// than a comparison of resolved values.
///
/// Carries no runtime data on purpose: the typed character (text entry) and the
/// viewport page size (page motions) are injected only by [`Command::resolve`], so
/// the same `Command` value is valid in any frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Save,
    SaveAs,
    Undo,
    Redo,
    DeleteBackward,
    DeleteForward,
    InsertNewline,
    InsertTab,
    AddCursorAbove,
    AddCursorBelow,
    CollapseSelections,
    Copy,
    Cut,
    Paste,
    /// A cursor motion; `extend` grows the selection (the `select_*` names).
    Move {
        kind: MoveKind,
        extend: bool,
    },
    /// Open the command palette overlay (frontend-local, never crosses the seam).
    OpenPalette,
    /// Open the fuzzy file-picker overlay (frontend-local).
    OpenFilePicker,
    /// Open the theme-picker overlay (frontend-local).
    OpenThemePicker,
    /// Open the buffer-picker overlay (frontend-local).
    OpenBufferPicker,
    /// Open the global-search picker (frontend-local: the walk, the matching and the
    /// results are all this side; only a picked hit becomes an `Action`).
    OpenSearchPicker,
    /// Swap the gutter between absolute and relative numbering (frontend-local: the
    /// core has no idea a margin exists). Config sets the mode you *start* in; this
    /// is the same switch reachable mid-session, since which numbering helps depends
    /// on what you are doing rather than on the machine you are on.
    ToggleLineNumbers,
    /// Draw or stop drawing the indent guides (frontend-local: the guides are derived
    /// from the text's own whitespace, so the core is not involved in showing them).
    /// Config sets what you start with, the same as the gutter's numbering mode.
    ToggleIndentGuides,
    /// Show or hide the scrollbar (frontend-local: the viewport is this side's, so
    /// what stands for it is too).
    ToggleScrollbar,
    /// Show or hide the sticky context header (frontend-local: the viewport is this
    /// side's, and so is the decision to spend rows of it on where you are).
    ToggleStickyContext,
    /// Open the encoding picker (frontend-local until a pick, which commits an
    /// `Action::SetEncoding`).
    OpenEncodingPicker,
    /// Open the line-ending picker (frontend-local until a pick).
    OpenLineEndingPicker,
    /// Focus the next buffer in the bufferline, wrapping at the end.
    ///
    /// Frontend-local *resolution*, not a frontend-local effect: the core has no
    /// "next" action, because the frontend already holds the ordered buffer list to
    /// paint its bufferline and so can name the neighbor itself (SPEC §7.5 - only the
    /// committed intent, `SwitchBuffer { id }`, crosses the seam).
    NextBuffer,
    /// Focus the previous buffer, wrapping at the start. See [`Command::NextBuffer`].
    PrevBuffer,
    /// Close the active buffer. Resolved against the live buffer list like the two
    /// above; the core refuses if there is unsaved work, and the frontend then asks.
    CloseBuffer,
    /// Open the in-buffer find prompt (SPEC §11) - frontend-local until Enter, since
    /// the preview needs nothing the frontend does not already hold.
    OpenFind,
    /// Open the in-buffer find-and-replace prompt: the same surface with a second
    /// field, committing into the query-replace walk.
    OpenReplace,
    /// Go to the next match of the last pattern searched for. Frontend-local
    /// *resolution* like [`Command::NextBuffer`]: the frontend is what remembers the
    /// pattern, so it fills it in and only the complete `SelectNextMatch` crosses.
    FindNext,
    /// Go to the previous match of the last pattern. See [`Command::FindNext`].
    FindPrevious,
    /// Put a cursor on every match of the last pattern (SPEC §12.2).
    SelectAllMatches,
    /// Commit the open surface: pick the highlighted row, submit the typed value,
    /// run the search (`picker`, `prompt`, `find`).
    Accept,
    /// Dismiss the open surface without committing (`picker`, `prompt`, `find`,
    /// `confirm`, `replace`).
    Cancel,
    /// Move a picker's highlight down (`picker`).
    NextItem,
    /// Move a picker's highlight up (`picker`).
    PreviousItem,
    /// Move to the replace prompt's second field (`find`).
    NextField,
    /// Answer a confirmation yes - the one answer that commits (`confirm`).
    ConfirmYes,
    /// Answer a confirmation no (`confirm`). Every unbound printable key means this
    /// too; the binding is what gives the answer a name to show and to rebind.
    ConfirmNo,
    /// Replace the match under the caret and walk on (`replace`).
    ReplaceYes,
    /// Leave the match alone and walk on (`replace`).
    ReplaceNo,
    /// Replace this match and every one after it, in a single edit (`replace`).
    ReplaceAll,
    /// Stop the walk (`replace`).
    ReplaceQuit,
    /// Do nothing - and, the part that matters, **consume the chord**.
    ///
    /// This is how a config unbinds: `"ctrl+f" = "nop"` frees the chord. Leaving the
    /// row out instead would leave the built-in binding in place, and there is nothing
    /// else to write - TOML has no null, so the absence of a command has to be spelled
    /// as one (Helix's `no_op`, Zed's `null`). Consuming is what separates it from an
    /// unbound key: [`command_for_key`]'s text-entry fallback would otherwise turn a
    /// freed printable chord back into typing.
    ///
    /// Under contexts it is also how a table says "swallow this **here**", as distinct
    /// from letting the chord fall through to the context below.
    Nop,
}

/// A motion with the page size left abstract, so a binding is frame-independent;
/// [`MoveKind::motion`] injects the runtime page for the page motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    BufferStart,
    BufferEnd,
}

impl MoveKind {
    /// The core [`Motion`], with `page` folded into the page motions (SPEC §5: page
    /// size is the viewport height, known only to the frontend).
    fn motion(self, page: usize) -> Motion {
        match self {
            MoveKind::Left => Motion::Left,
            MoveKind::Right => Motion::Right,
            MoveKind::Up => Motion::Up,
            MoveKind::Down => Motion::Down,
            MoveKind::LineStart => Motion::LineStart,
            MoveKind::LineEnd => Motion::LineEnd,
            MoveKind::PageUp => Motion::PageUp(page),
            MoveKind::PageDown => Motion::PageDown(page),
            MoveKind::BufferStart => Motion::BufferStart,
            MoveKind::BufferEnd => Motion::BufferEnd,
        }
    }
}

impl Command {
    /// Parse a command name. Motions use a `move_<kind>` / `select_<kind>` scheme
    /// (`select_` is the selection-extending variant), e.g. `move_line_start`,
    /// `select_page_down`. Returns `None` for an unknown name.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        // The motion prefixes are tried first but do **not** claim the name: a
        // `select_` that is not a motion (`select_all_matches`) falls through to the
        // table below rather than being rejected as a bad motion. Reserving the whole
        // prefix would make the naming scheme a trap for every future selection
        // command that is not a movement.
        if let Some(rest) = name.strip_prefix("move_")
            && let Some(kind) = parse_move_kind(rest)
        {
            return Some(Command::Move {
                kind,
                extend: false,
            });
        }
        if let Some(rest) = name.strip_prefix("select_")
            && let Some(kind) = parse_move_kind(rest)
        {
            return Some(Command::Move { kind, extend: true });
        }
        Some(match name {
            "quit" => Command::Quit,
            "save" => Command::Save,
            "save_as" => Command::SaveAs,
            "undo" => Command::Undo,
            "redo" => Command::Redo,
            "delete_backward" => Command::DeleteBackward,
            "delete_forward" => Command::DeleteForward,
            "insert_newline" => Command::InsertNewline,
            "insert_tab" => Command::InsertTab,
            "add_cursor_above" => Command::AddCursorAbove,
            "add_cursor_below" => Command::AddCursorBelow,
            "collapse_selections" => Command::CollapseSelections,
            "copy" => Command::Copy,
            "cut" => Command::Cut,
            "paste" => Command::Paste,
            "open_palette" => Command::OpenPalette,
            "open_file_picker" => Command::OpenFilePicker,
            "open_theme_picker" => Command::OpenThemePicker,
            "open_buffer_picker" => Command::OpenBufferPicker,
            "open_search_picker" => Command::OpenSearchPicker,
            "toggle_line_numbers" => Command::ToggleLineNumbers,
            "toggle_indent_guides" => Command::ToggleIndentGuides,
            "toggle_scrollbar" => Command::ToggleScrollbar,
            "toggle_sticky_context" => Command::ToggleStickyContext,
            "open_encoding_picker" => Command::OpenEncodingPicker,
            "open_line_ending_picker" => Command::OpenLineEndingPicker,
            "next_buffer" => Command::NextBuffer,
            "prev_buffer" => Command::PrevBuffer,
            "close_buffer" => Command::CloseBuffer,
            "find" => Command::OpenFind,
            "replace" => Command::OpenReplace,
            "find_next" => Command::FindNext,
            "find_previous" => Command::FindPrevious,
            "select_all_matches" => Command::SelectAllMatches,
            "accept" => Command::Accept,
            "cancel" => Command::Cancel,
            "next_item" => Command::NextItem,
            "previous_item" => Command::PreviousItem,
            "next_field" => Command::NextField,
            "confirm_yes" => Command::ConfirmYes,
            "confirm_no" => Command::ConfirmNo,
            "replace_yes" => Command::ReplaceYes,
            "replace_no" => Command::ReplaceNo,
            "replace_all" => Command::ReplaceAll,
            "replace_quit" => Command::ReplaceQuit,
            "nop" => Command::Nop,
            _ => return None,
        })
    }

    /// Whether this command may be bound in `context`.
    ///
    /// Without the check, `next_item` written in `[keys]` would be a row that parses,
    /// applies, and never fires - the silent failure SPEC §8 forbids. It is also what
    /// makes "the palette lists editor commands" a rule rather than an oversight: a
    /// surface command is unrunnable from the one place its surface is not open.
    ///
    /// Shared names where the meaning is shared: `delete_backward` is one intent over
    /// whatever field has focus, so it is the editor's *and* every text surface's.
    fn allowed_in(self, context: Context) -> bool {
        let scope = context.scope();
        match self {
            // Every context: unbinding has to be possible wherever binding is.
            Command::Nop => true,
            Command::Accept => {
                matches!(scope, Context::Picker | Context::Prompt | Context::Find)
            }
            Command::Cancel => matches!(
                scope,
                Context::Picker
                    | Context::Prompt
                    | Context::Find
                    | Context::Confirm
                    | Context::Replace
            ),
            Command::DeleteBackward => matches!(
                scope,
                Context::Editor | Context::Picker | Context::Prompt | Context::Find
            ),
            Command::NextItem | Command::PreviousItem => scope == Context::Picker,
            Command::NextField => scope == Context::Find,
            Command::ConfirmYes | Command::ConfirmNo => scope == Context::Confirm,
            Command::ReplaceYes
            | Command::ReplaceNo
            | Command::ReplaceAll
            | Command::ReplaceQuit => scope == Context::Replace,
            // Everything else is the editor's, and only the editor's.
            _ => scope == Context::Editor,
        }
    }

    /// Finalize into the dispatchable command for this frame (`page` sizes page
    /// motions). Overlay triggers resolve to a frontend-local command; everything
    /// else wraps a core [`Action`] for the seam. `None` for [`Command::Nop`], which
    /// is the one command that dispatches nothing.
    pub fn resolve(self, page: usize) -> Option<FrontendCommand> {
        let action = match self {
            Command::Nop => return None,
            // A surface command is dispatched by the surface that was handed it, not
            // by the loop - the layer holds the query, the highlight and the walk this
            // would have to act on. It cannot reach here anyway: `allowed_in` keeps it
            // out of every context the editor is looked up in.
            Command::Accept
            | Command::Cancel
            | Command::NextItem
            | Command::PreviousItem
            | Command::NextField
            | Command::ConfirmYes
            | Command::ConfirmNo
            | Command::ReplaceYes
            | Command::ReplaceNo
            | Command::ReplaceAll
            | Command::ReplaceQuit => return None,
            Command::OpenPalette => return Some(FrontendCommand::OpenPalette),
            Command::OpenFilePicker => return Some(FrontendCommand::OpenFilePicker),
            Command::OpenThemePicker => return Some(FrontendCommand::OpenThemePicker),
            Command::OpenBufferPicker => return Some(FrontendCommand::OpenBufferPicker),
            Command::OpenSearchPicker => return Some(FrontendCommand::OpenSearchPicker),
            Command::ToggleLineNumbers => return Some(FrontendCommand::ToggleLineNumbers),
            Command::ToggleIndentGuides => return Some(FrontendCommand::ToggleIndentGuides),
            Command::ToggleScrollbar => return Some(FrontendCommand::ToggleScrollbar),
            Command::ToggleStickyContext => return Some(FrontendCommand::ToggleStickyContext),
            Command::OpenEncodingPicker => return Some(FrontendCommand::OpenEncodingPicker),
            Command::OpenLineEndingPicker => return Some(FrontendCommand::OpenLineEndingPicker),
            Command::SaveAs => return Some(FrontendCommand::OpenSavePrompt),
            // Resolved against the live buffer list at dispatch, where it is known.
            Command::NextBuffer => return Some(FrontendCommand::NextBuffer),
            Command::PrevBuffer => return Some(FrontendCommand::PrevBuffer),
            Command::CloseBuffer => return Some(FrontendCommand::CloseBuffer),
            // Search, all resolved at dispatch: the prompts are frontend surfaces,
            // and the repeat commands need the pattern only the frontend remembers.
            Command::OpenFind => {
                return Some(FrontendCommand::OpenFindPrompt { replacing: false });
            }
            Command::OpenReplace => {
                return Some(FrontendCommand::OpenFindPrompt { replacing: true });
            }
            Command::FindNext => return Some(FrontendCommand::FindNext),
            Command::FindPrevious => return Some(FrontendCommand::FindPrevious),
            Command::SelectAllMatches => return Some(FrontendCommand::SelectAllMatches),
            Command::Quit => Action::Quit,
            Command::Save => Action::Save { force: false },
            Command::Undo => Action::Undo,
            Command::Redo => Action::Redo,
            Command::DeleteBackward => Action::DeleteBackward,
            Command::DeleteForward => Action::DeleteForward,
            Command::InsertNewline => Action::Insert("\n".to_string()),
            Command::InsertTab => Action::Insert("\t".to_string()),
            Command::AddCursorAbove => Action::AddCursorAbove,
            Command::AddCursorBelow => Action::AddCursorBelow,
            Command::CollapseSelections => Action::CollapseSelections,
            Command::Copy => Action::Copy,
            Command::Cut => Action::Cut,
            Command::Paste => Action::Paste,
            Command::Move { kind, extend } => Action::MoveCursor {
                motion: kind.motion(page),
                extend,
            },
        };
        Some(FrontendCommand::Editor(action))
    }
}

/// A move-kind name (the suffix of a `move_`/`select_` command) to its [`MoveKind`].
fn parse_move_kind(name: &str) -> Option<MoveKind> {
    Some(match name {
        "left" => MoveKind::Left,
        "right" => MoveKind::Right,
        "up" => MoveKind::Up,
        "down" => MoveKind::Down,
        "line_start" => MoveKind::LineStart,
        "line_end" => MoveKind::LineEnd,
        "page_up" => MoveKind::PageUp,
        "page_down" => MoveKind::PageDown,
        "buffer_start" => MoveKind::BufferStart,
        "buffer_end" => MoveKind::BufferEnd,
        _ => return None,
    })
}

/// The built-in bindings, compiled in from the file that *is* the default keymap.
///
/// A file rather than a Rust table for two reasons. It answers a question the editor
/// could not answer before - *what are the defaults?* - since it is the reference a
/// user copies a row out of. And it deletes the possibility of drift: there is no
/// second copy to keep in step, so no equality test is needed. Unlike
/// `Theme::default()`, which keeps its hand-written twin because a theme must exist
/// before any parsing can fail, a keymap has no such bootstrap need - a `keys.toml`
/// that will not parse is a build-time bug, and [`Keymap::default`]'s own test is what
/// catches it.
const BUILTIN_KEYS: &str = include_str!("../keys.toml");

/// Parse a built-in keymap file. The same `toml` shape a user's `[keys]` table has,
/// so the defaults reach [`Keymap::extend_from_table`] by exactly the path a config
/// file takes.
///
/// Panics on a malformed file, which is a build-time bug in a compiled-in constant
/// rather than anything a user can cause - the invariant [`Keymap::default`]'s test
/// proves.
fn builtin_table(text: &str) -> toml::Table {
    toml::from_str(text).expect("built-in keymap file must be a valid keys table")
}

/// The resolved key bindings, per context. Opaque so its representation can change
/// (e.g. gain chord *sequences*) without touching call sites.
///
/// One table for every binding - edit, overlay trigger and surface key alike - keyed
/// by `(context, chord)`. A map per context would be a second thing every loader has
/// to remember to populate, and the config path goes through
/// [`Keymap::extend_from_table`] only, so anything it missed would silently vanish
/// the first time a user wrote a config file.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<(Context, Chord), Command>,
}

impl Keymap {
    /// Build an **editor-context** keymap from `(chord, command)` string pairs.
    ///
    /// Not the config path - that is [`Keymap::extend_from_table`], which is the only
    /// loader a `[keys]` table reaches and the only one that can express a context.
    /// This is the terse constructor for a keymap with a handful of editor rows and
    /// nothing else, which is what most of this module's tests want.
    ///
    /// # Errors
    /// Returns [`KeymapError`] naming the first unusable row, so a bad line is
    /// surfaced rather than silently dropped (SPEC §8).
    pub fn from_pairs<'a, I>(pairs: I) -> Result<Self, KeymapError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut keymap = Self {
            bindings: HashMap::new(),
        };
        for (chord, command) in pairs {
            keymap.bind(Context::Editor, chord, command)?;
        }
        Ok(keymap)
    }

    /// Apply a `[keys]` table over the existing bindings - the shape a config file
    /// writes and the shape `keys.toml` is written in (SPEC §10.5).
    ///
    /// A **string** value is a binding in the editor context; a **table** value is a
    /// context, and its rows are bindings in it. Contexts do not nest, so a table
    /// inside a table is an error. Rebinding a chord that is already bound in that
    /// context replaces it, which is the point; every chord left alone keeps working,
    /// so a user who rebinds one key does not lose the other fifty.
    ///
    /// # Errors
    /// Returns one error per bad row and applies **every** binding that did parse.
    ///
    /// Deliberately not "stop at the first bad one". That version's promise - a typo
    /// on line 9 leaves lines 1 to 8 applied - was never true, because the rows arrive
    /// from a `toml` table and so in *alphabetical* order rather than file order: a bad
    /// `ctrl+a` discarded a good `ctrl+z` written above it. Applying what parses makes
    /// the outcome independent of the order entirely, which is the only version of the
    /// promise that survives not controlling that order, and it is what the user wants
    /// anyway - one typo should cost one binding.
    ///
    /// `#[must_use]` by hand: dropping to a plain `Vec` would otherwise let a caller
    /// discard every binding error and leave a user's typo silently unbound (SPEC §8).
    #[must_use]
    pub fn extend_from_table(&mut self, table: &toml::Table) -> Vec<KeymapError> {
        let mut rejected = Vec::new();
        for (key, value) in table {
            // A table is a context and its rows are the bindings; anything else is a
            // row of the editor's own table, `key` being the chord.
            let (context, rows) = match value {
                toml::Value::Table(rows) => match Context::parse(key) {
                    Some(context) => (context, rows.iter()),
                    None => {
                        rejected.push(KeymapError::UnknownContext(key.clone()));
                        continue;
                    }
                },
                _ => {
                    rejected.extend(self.bind_value(Context::Editor, key, value).err());
                    continue;
                }
            };
            for (chord, value) in rows {
                rejected.extend(self.bind_value(context, chord, value).err());
            }
        }
        rejected
    }

    /// Bind one row whose command side is still a `toml` value.
    ///
    /// A value that is not a string has no reading as a binding - under a context that
    /// means a nested table, which contexts do not have, and at the top level it means
    /// a number or a list where a command name belongs. Both are typos, and saying so
    /// beats binding nothing quietly (SPEC §8).
    fn bind_value(
        &mut self,
        context: Context,
        chord: &str,
        value: &toml::Value,
    ) -> Result<(), KeymapError> {
        match value.as_str() {
            Some(command) => self.bind(context, chord, command),
            None => Err(KeymapError::NotABinding {
                chord: chord.to_string(),
                kind: value.type_str(),
            }),
        }
    }

    /// Bind one row, or say what was wrong with it.
    fn bind(&mut self, context: Context, chord: &str, command: &str) -> Result<(), KeymapError> {
        let chord_key =
            Chord::parse(chord).ok_or_else(|| KeymapError::UnknownChord(chord.to_string()))?;
        let parsed = Command::parse(command)
            .ok_or_else(|| KeymapError::UnknownCommand(command.to_string()))?;
        if !parsed.allowed_in(context) {
            return Err(KeymapError::WrongContext {
                command: command.to_string(),
                context,
            });
        }
        self.bindings.insert((context, chord_key), parsed);
        Ok(())
    }

    /// The command `key` is bound to in `context`, or `None` if that context leaves
    /// the chord alone. One context, not the stack: a [`crate::compositor::Layer`]
    /// asks about its own, and a chord it does not bind falls through by the layer
    /// declining the key, which is how the next context down gets its turn.
    ///
    /// Key **releases** answer `None` - the Kitty protocol reports them (SPEC §9) and
    /// acting on both edges would double-fire every binding.
    pub fn bound(&self, context: Context, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        self.bindings
            .get(&(context, Chord::from_event(&key)))
            .copied()
    }

    /// The shortcut bound to `command` in `context`, formatted for display (e.g.
    /// `"Ctrl+S"`), or `None` if it is unbound there. This is the **only** way a key
    /// name is rendered anywhere (SPEC §10.5): a binding the user can change is one
    /// the editor must not hard-code into the text it shows.
    ///
    /// [`Context::Editor`] also searches the platform context, because the platform is
    /// part of the editor's own stack rather than a surface of its own - without it a
    /// command bound only in `[keys.macos]` would have no chord to show on a Mac.
    ///
    /// Matched on the command **identity**, so the lookup is exact: comparing
    /// *resolved* values instead would need a page size to resolve against, and any
    /// command carrying runtime data would silently stop matching (no error - the
    /// shortcut would just stop appearing).
    ///
    /// A command may have several bindings (on macOS Quit is both Ctrl+Q and Ctrl+C);
    /// `max` picks one **deterministically** - HashMap order is not stable - and
    /// happens to prefer Ctrl+Q over Ctrl+C.
    pub fn shortcut_for(&self, command: Command, context: Context) -> Option<String> {
        let searched: &[Context] = if context == Context::Editor {
            &Context::EDITOR_STACK
        } else {
            std::slice::from_ref(&context)
        };
        self.bindings
            .iter()
            .filter(|((ctx, _), bound)| searched.contains(ctx) && **bound == command)
            .map(|((_, chord), _)| chord.display())
            .max()
    }
}

impl Default for Keymap {
    /// The built-in keymap: [`BUILTIN_KEYS`], read through the same loader a user's
    /// `[keys]` table goes through.
    ///
    /// No `cfg!` left. The platform-only rows live in the file's own `[macos]` table
    /// and are loaded everywhere; the platform *context* is what is active on one OS
    /// and not the others, which is the whole of what is left of the platform split
    /// (`mod` carries the rest - see [`Chord::parse`]).
    ///
    /// Parsing cannot fail - the file is a compiled-in constant covered by a test - so
    /// the `expect` is invariant-proven.
    fn default() -> Self {
        let mut keymap = Self {
            bindings: HashMap::new(),
        };
        let rejected = keymap.extend_from_table(&builtin_table(BUILTIN_KEYS));
        assert!(
            rejected.is_empty(),
            "built-in default bindings must be valid: {rejected:?}"
        );
        keymap
    }
}

/// A row of a keys table that could not be applied, naming what was wrong with it so
/// the user can fix their config. Carries no source location yet (M5 adds line context
/// on file load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    UnknownChord(String),
    UnknownCommand(String),
    /// A subtable of `[keys]` that names no context. The set is closed, so this is a
    /// typo rather than a table for a surface that does not exist yet (SPEC §8).
    UnknownContext(String),
    /// A command that parses but cannot fire where it was written - `next_item` in
    /// `[keys]`, say. Reported rather than applied, because a binding that never fires
    /// is the silent failure SPEC §8 forbids.
    WrongContext {
        command: String,
        context: Context,
    },
    /// A value that is neither a command name nor a context table.
    NotABinding {
        chord: String,
        kind: &'static str,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::UnknownChord(s) => write!(f, "unknown key chord `{s}`"),
            KeymapError::UnknownCommand(s) => write!(f, "unknown command `{s}`"),
            KeymapError::UnknownContext(s) => write!(f, "unknown key context `{s}`"),
            KeymapError::WrongContext { command, context } => write!(
                f,
                "command `{command}` cannot be bound in the `{}` context",
                context.name()
            ),
            KeymapError::NotABinding { chord, kind } => {
                write!(f, "`{chord}` is bound to a {kind}, not a command name")
            }
        }
    }
}

impl std::error::Error for KeymapError {}

/// Translate a key event into the [`FrontendCommand`] the event loop dispatches
/// (SPEC §7.5), or `None` if the key is unmapped.
///
/// The **editor's** end of the lookup: the platform context over the editor one,
/// walked from the top down, which is the stack that is active whenever no overlay
/// holds the key. An overlay's own context is resolved by the compositor
/// ([`Keymap::bound`]) and only reaches here when the layer declines the key.
///
/// One lookup for every binding, edit and overlay trigger alike - the routing decision
/// lives in the command a chord names, not in which table it was found in. Only key
/// **press** (and repeat) events map; releases are ignored so the Kitty protocol's
/// release reporting (SPEC §9) does not double-fire edits. `page` sizes any page
/// motion. An unbound **printable char** with no Ctrl falls back to inserting itself,
/// so ordinary typing needs no per-letter binding.
pub fn command_for_key(keymap: &Keymap, key: KeyEvent, page: usize) -> Option<FrontendCommand> {
    // With the Kitty protocol enabled we receive Release events too; act only on
    // Press/Repeat. (Classic terminals only ever send Press, so this is safe.)
    if key.kind == KeyEventKind::Release {
        return None;
    }

    // Top down: the platform's row wins over the editor's on the same chord.
    for context in Context::EDITOR_STACK.iter().rev() {
        if let Some(command) = keymap.bound(*context, key) {
            // A bound chord is answered here whatever it resolves to - including
            // `nop`, which resolves to nothing. Returning is what *consumes* it:
            // falling through to the text-entry fallback below would turn a freed
            // printable chord back into typing, the opposite of unbinding it.
            return command.resolve(page);
        }
    }

    // Text-entry fallback: an unbound printable char inserts itself. A Ctrl- or
    // Cmd-modified char is a command chord, never text, so it is not inserted -
    // otherwise an unbound Cmd+S / Ctrl+A would type a literal `s`/`a`. (Alt is
    // deliberately allowed through: on many layouts Alt/Option composes accented
    // characters that are legitimate text.)
    text_key(&key).map(|c| FrontendCommand::Editor(Action::Insert(c.to_string())))
}

/// The character an **unbound** key types, or `None` if it is not text at all.
///
/// The one test every text-taking surface applies to a key its context did not bind:
/// the editor inserts it, a picker filters by it, a prompt types it. A Ctrl- or
/// Cmd-modified char is a command chord, never text - otherwise an unbound Cmd+S would
/// type a literal `s`. (Alt is deliberately allowed through: on many layouts
/// Alt/Option composes accented characters that are legitimate text.)
///
/// One function rather than the same condition in five files, because the five must
/// agree: a surface that disagreed would take a shortcut as typing. Reads `key.code`
/// rather than the chord, so a shifted letter types its upper-case form - the chord
/// folds the case away, which is a lookup concern rather than a typing one.
pub fn text_key(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if !is_command_chord(key) => Some(c),
        _ => None,
    }
}

/// Whether `key` carries a **command modifier** - Ctrl, or Cmd on a Mac.
///
/// The test a surface applies to a key its context did not bind when its miss policy
/// is "decline": a command chord is somebody's shortcut and is offered to the context
/// below, while everything else is an answer of "no". Without the distinction a
/// confirmation would either swallow Ctrl+S (so a shortcut stops working the moment a
/// question is up) or defer Backspace (so answering a question edits the buffer behind
/// it). Alt is not a command modifier here, for the reason [`text_key`] gives.
pub fn is_command_chord(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::SUPER)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Page size used by the tests that do not exercise PageUp/PageDown; a fixed,
    /// arbitrary value keeps the non-page assertions independent of it.
    const PAGE: usize = 10;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Translate a key against the default keymap with the fixed test [`PAGE`],
    /// keeping only the core intent - the shape most of these assertions care about.
    /// A key that resolves to a frontend-local command yields `None` here.
    fn act(key: KeyEvent) -> Option<Action> {
        act_on(&Keymap::default(), key, PAGE)
    }

    /// [`act`] against a specific keymap and page size.
    fn act_on(keymap: &Keymap, key: KeyEvent, page: usize) -> Option<Action> {
        match command_for_key(keymap, key, page) {
            Some(FrontendCommand::Editor(action)) => Some(action),
            _ => None,
        }
    }

    #[test]
    fn default_keymap_builds_without_panicking() {
        // Guards the `expect` in `Keymap::default` - proves keys.toml parses.
        let _ = Keymap::default();
    }

    /// How many `chord = command` rows `keys.toml` spells out, counted from the *file*
    /// rather than from the map it produced - which is the whole point of the caller
    /// below, since a row that collapsed onto another is exactly what it looks for.
    fn builtin_row_count() -> usize {
        builtin_table(BUILTIN_KEYS)
            .values()
            .map(|value| match value {
                toml::Value::Table(rows) => rows.len(),
                _ => 1,
            })
            .sum()
    }

    #[test]
    fn the_built_in_file_binds_every_context_the_closed_set_names() {
        // That each row *parses in the context it is written in* is not asserted here:
        // `Keymap::default` runs the real loader and panics with the whole rejected
        // list, so a bad chord, an unknown command, an unknown context or a command
        // outside its scope already fails `default_keymap_builds_without_panicking`.
        // Re-checking it row by row would be a second loader to keep in step.
        //
        // What that cannot catch is a table nobody wrote. Every context is checked on
        // every platform - the `[macos]` table is read everywhere precisely so a typo
        // in it cannot wait for a Mac to be found.
        let km = Keymap::default();
        for context in [
            Context::Editor,
            Context::Macos,
            Context::Picker,
            Context::Prompt,
            Context::Find,
            Context::Confirm,
            Context::Replace,
        ] {
            assert!(
                km.bindings.keys().any(|(ctx, _)| *ctx == context),
                "keys.toml binds nothing in the `{}` context",
                context.name()
            );
        }
        assert!(
            builtin_row_count() > 50,
            "keys.toml bound only {}",
            builtin_row_count()
        );
    }

    #[test]
    fn no_two_built_in_rows_resolve_to_the_same_chord() {
        // Two rows can spell one chord - `mod+c` and `ctrl+c` are the same key off a
        // Mac - and the loser then vanishes silently, with *which* one loses decided by
        // the alphabetical order `toml` hands the table back in. That is exactly the
        // ordering `extend_from_table` refuses to depend on, so the built-ins are held
        // to never needing it: every row must survive into the map.
        assert_eq!(
            Keymap::default().bindings.len(),
            builtin_row_count(),
            "two built-in rows collapsed onto one chord"
        );
    }

    #[test]
    fn the_macos_table_is_active_only_on_a_mac() {
        // Its one row keeps Ctrl+C on quit where the clipboard sits on Cmd. Elsewhere
        // `mod+c` *is* Ctrl+C, so applying it would silently take copy's chord away.
        // The row is *loaded* on every platform now; what changes is which context is
        // in the editor's stack, which is the whole of the platform split.
        let km = Keymap::default();
        let ctrl_c = with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(km.bound(Context::Macos, ctrl_c), Some(Command::Quit));
        // The editor's own `mod+c` *is* Ctrl+C off a Mac, and Cmd+C on one - so what
        // the `[macos]` row shadows there is nothing at all.
        assert_eq!(
            km.bound(Context::Editor, ctrl_c),
            (!cfg!(target_os = "macos")).then_some(Command::Copy)
        );
        assert_eq!(
            act(ctrl_c),
            Some(if cfg!(target_os = "macos") {
                Action::Quit
            } else {
                Action::Copy
            })
        );
    }

    #[test]
    fn mod_is_the_platform_command_modifier() {
        // One row that means the right thing on both platforms: Cmd on macOS, Ctrl
        // elsewhere - resolved at parse time, so nothing downstream sees a `mod`.
        let chord = Chord::parse("mod+c").expect("mod is a modifier token");
        assert_eq!(chord.code, KeyCode::Char('c'));
        assert_eq!(
            (chord.cmd, chord.ctrl),
            (cfg!(target_os = "macos"), !cfg!(target_os = "macos"))
        );
        // And it displays as the key the user actually presses, which is the whole
        // reason it resolves at parse time rather than at lookup.
        assert_eq!(
            chord.display(),
            if cfg!(target_os = "macos") {
                "Cmd+C"
            } else {
                "Ctrl+C"
            }
        );
        // It composes with the other modifiers, and is not a key of its own.
        assert_eq!(Chord::parse("mod+shift+f").map(|c| c.shift), Some(true));
        assert_eq!(Chord::parse("mod"), None);
    }

    #[test]
    fn a_mod_binding_from_a_config_fires_on_the_platform_chord() {
        // The user-facing half of `mod`: a config file writes one row and gets the
        // native chord, with no per-OS branch of its own.
        let km = Keymap::from_pairs([("mod+e", "quit")]).unwrap();
        assert_eq!(
            act_on(&km, with_mods(KeyCode::Char('e'), CMD_MOD), PAGE),
            Some(Action::Quit)
        );
    }

    #[test]
    fn nop_unbinds_a_chord_and_consumes_it() {
        // `nop` frees a chord: Ctrl+F stops opening the find prompt.
        let km = Keymap::from_pairs([("ctrl+f", "nop"), ("f", "nop")]).unwrap();
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('f'), KeyModifiers::CONTROL),
                PAGE
            ),
            None
        );
        // And the part that matters: a *printable* chord bound to nop is consumed,
        // not handed to the text-entry fallback. Unbinding `f` must not make it type
        // itself - that would be the opposite of what the user asked for.
        assert_eq!(command_for_key(&km, press(KeyCode::Char('f')), PAGE), None);
        // The chord next to it still types, so this is an unbind and not a mute.
        assert_eq!(
            command_for_key(&km, press(KeyCode::Char('g')), PAGE),
            Some(FrontendCommand::Editor(Action::Insert("g".into())))
        );
    }

    #[test]
    fn nop_is_a_command_name_like_any_other() {
        assert_eq!(Command::parse("nop"), Some(Command::Nop));
        assert_eq!(Command::Nop.resolve(PAGE), None);
        // It is never bound by default, so nothing in the built-in map disappears.
        assert_eq!(
            Keymap::default().shortcut_for(Command::Nop, Context::Editor),
            None
        );
    }

    #[test]
    fn buffer_switching_is_bound_and_stays_frontend_local() {
        // Next/previous/close resolve to frontend-local commands, not core actions:
        // the core has no notion of "next", so the frontend names the buffer from the
        // list it already holds and only `SwitchBuffer { id }` crosses the seam.
        let km = Keymap::default();
        let page = PAGE;
        let ctrl = |code| with_mods(code, KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&km, ctrl(KeyCode::PageDown), page),
            Some(FrontendCommand::NextBuffer)
        );
        assert_eq!(
            command_for_key(&km, ctrl(KeyCode::PageUp), page),
            Some(FrontendCommand::PrevBuffer)
        );
        assert_eq!(
            command_for_key(&km, ctrl(KeyCode::Char('w')), page),
            Some(FrontendCommand::CloseBuffer)
        );
        assert_eq!(
            command_for_key(&km, ctrl(KeyCode::Char('b')), page),
            Some(FrontendCommand::OpenBufferPicker)
        );
    }

    #[test]
    fn the_plain_page_keys_still_move_the_cursor() {
        // Ctrl+PageUp/PageDown were added for buffer switching; the unmodified and
        // shift+ page keys must be unaffected.
        assert_eq!(
            act(press(KeyCode::PageDown)),
            Some(Action::MoveCursor {
                motion: Motion::PageDown(PAGE),
                extend: false,
            })
        );
        assert_eq!(
            act(with_mods(KeyCode::PageUp, KeyModifiers::SHIFT)),
            Some(Action::MoveCursor {
                motion: Motion::PageUp(PAGE),
                extend: true,
            })
        );
    }

    #[test]
    fn buffer_command_names_parse_and_have_shortcuts() {
        assert_eq!(Command::parse("next_buffer"), Some(Command::NextBuffer));
        assert_eq!(Command::parse("prev_buffer"), Some(Command::PrevBuffer));
        assert_eq!(Command::parse("close_buffer"), Some(Command::CloseBuffer));
        // The palette shows these, so the reverse lookup has to find them.
        let km = Keymap::default();
        assert!(
            km.shortcut_for(Command::NextBuffer, Context::Editor)
                .is_some()
        );
        assert!(
            km.shortcut_for(Command::PrevBuffer, Context::Editor)
                .is_some()
        );
        assert!(
            km.shortcut_for(Command::CloseBuffer, Context::Editor)
                .is_some()
        );
    }

    #[test]
    fn plain_char_inserts() {
        assert_eq!(
            act(press(KeyCode::Char('a'))),
            Some(Action::Insert("a".into()))
        );
    }

    #[test]
    fn uppercase_char_inserts_its_case() {
        // Shift+letter arrives as the uppercase char; the fallback preserves case.
        assert_eq!(
            act(with_mods(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some(Action::Insert("A".into()))
        );
    }

    #[test]
    fn enter_and_tab_insert_whitespace() {
        assert_eq!(
            act(press(KeyCode::Enter)),
            Some(Action::Insert("\n".into()))
        );
        assert_eq!(act(press(KeyCode::Tab)), Some(Action::Insert("\t".into())));
    }

    #[test]
    fn backspace_and_delete() {
        assert_eq!(act(press(KeyCode::Backspace)), Some(Action::DeleteBackward));
        assert_eq!(act(press(KeyCode::Delete)), Some(Action::DeleteForward));
    }

    #[test]
    fn arrows_map_to_motions_without_extend() {
        assert_eq!(
            act(press(KeyCode::Left)),
            Some(Action::MoveCursor {
                motion: Motion::Left,
                extend: false
            })
        );
        assert_eq!(
            act(press(KeyCode::Up)),
            Some(Action::MoveCursor {
                motion: Motion::Up,
                extend: false
            })
        );
    }

    #[test]
    fn shift_arrow_extends() {
        assert_eq!(
            act(with_mods(KeyCode::Right, KeyModifiers::SHIFT)),
            Some(Action::MoveCursor {
                motion: Motion::Right,
                extend: true
            })
        );
    }

    #[test]
    fn home_end_map_to_line_bounds() {
        assert_eq!(
            act(press(KeyCode::Home)),
            Some(Action::MoveCursor {
                motion: Motion::LineStart,
                extend: false
            })
        );
        assert_eq!(
            act(press(KeyCode::End)),
            Some(Action::MoveCursor {
                motion: Motion::LineEnd,
                extend: false
            })
        );
    }

    #[test]
    fn page_keys_carry_the_supplied_page_size() {
        // The keymap folds the caller's page size into the motion (SPEC §5).
        assert_eq!(
            act_on(&Keymap::default(), press(KeyCode::PageDown), 20),
            Some(Action::MoveCursor {
                motion: Motion::PageDown(20),
                extend: false
            })
        );
        assert_eq!(
            act_on(&Keymap::default(), press(KeyCode::PageUp), 20),
            Some(Action::MoveCursor {
                motion: Motion::PageUp(20),
                extend: false
            })
        );
    }

    #[test]
    fn shift_page_down_extends_selection() {
        assert_eq!(
            act_on(
                &Keymap::default(),
                with_mods(KeyCode::PageDown, KeyModifiers::SHIFT),
                15
            ),
            Some(Action::MoveCursor {
                motion: Motion::PageDown(15),
                extend: true
            })
        );
    }

    #[test]
    fn ctrl_q_always_quits() {
        assert_eq!(
            act(with_mods(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn ctrl_c_quits_on_macos_and_copies_elsewhere() {
        // Ctrl+C is platform-dependent: on macOS copy is Cmd+C, so Ctrl+C keeps its
        // terminal-conventional quit; elsewhere Ctrl+C is copy and quit is Ctrl+Q.
        let action = act(with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL));
        #[cfg(target_os = "macos")]
        assert_eq!(action, Some(Action::Quit));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(action, Some(Action::Copy));
    }

    #[test]
    fn platform_command_key_copies_cuts_and_pastes() {
        // Clipboard follows each OS: Cmd+C/X/V on macOS, Ctrl+C/X/V elsewhere.
        assert_eq!(
            act(with_mods(KeyCode::Char('c'), CMD_MOD)),
            Some(Action::Copy)
        );
        assert_eq!(
            act(with_mods(KeyCode::Char('x'), CMD_MOD)),
            Some(Action::Cut)
        );
        assert_eq!(
            act(with_mods(KeyCode::Char('v'), CMD_MOD)),
            Some(Action::Paste)
        );
    }

    #[test]
    fn clipboard_command_names_parse() {
        assert_eq!(Command::parse("copy"), Some(Command::Copy));
        assert_eq!(Command::parse("cut"), Some(Command::Cut));
        assert_eq!(Command::parse("paste"), Some(Command::Paste));
    }

    #[test]
    fn command_for_key_routes_ctrl_o_to_the_file_picker() {
        // Ctrl+O is a UI-overlay trigger, resolved through the keymap (SPEC §7.5) -
        // not an inline branch in the loop. It opens the fuzzy file picker.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(FrontendCommand::OpenFilePicker)
        );
        // Ctrl+T is the third overlay trigger, and rides the same table.
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('t'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(FrontendCommand::OpenThemePicker)
        );
        // Named like any other command, so a config file can rebind it.
        assert_eq!(
            Command::parse("open_theme_picker"),
            Some(Command::OpenThemePicker)
        );
        assert_eq!(
            km.shortcut_for(Command::OpenThemePicker, Context::Editor)
                .as_deref(),
            Some("Ctrl+T")
        );
    }

    #[test]
    fn the_line_number_toggle_is_nameable_but_unbound_by_default() {
        // Chrome switches are named commands first and chords only if they earn one.
        // This one is reached from the palette, so it costs no chord - but it parses
        // like any other, which is what lets a config file bind it (SPEC §10.5).
        let km = Keymap::default();
        assert_eq!(
            Command::parse("toggle_line_numbers"),
            Some(Command::ToggleLineNumbers)
        );
        assert_eq!(
            Command::ToggleLineNumbers.resolve(PAGE),
            Some(FrontendCommand::ToggleLineNumbers)
        );
        assert_eq!(
            km.shortcut_for(Command::ToggleLineNumbers, Context::Editor),
            None
        );
        // A config file can give it one, and the palette then shows that chord.
        let bound = Keymap::from_pairs([("ctrl+l", "toggle_line_numbers")]).unwrap();
        assert_eq!(
            bound
                .shortcut_for(Command::ToggleLineNumbers, Context::Editor)
                .as_deref(),
            Some("Ctrl+L")
        );
    }

    #[test]
    fn the_sticky_context_toggle_is_nameable_but_unbound_by_default() {
        // The same rule the other chrome switches follow: palette-reachable, config-
        // bindable, and unbound until it earns a chord (SPEC §10.5).
        assert_eq!(
            Command::parse("toggle_sticky_context"),
            Some(Command::ToggleStickyContext)
        );
        assert_eq!(
            Command::ToggleStickyContext.resolve(PAGE),
            Some(FrontendCommand::ToggleStickyContext)
        );
        assert_eq!(
            Keymap::default().shortcut_for(Command::ToggleStickyContext, Context::Editor),
            None
        );
    }

    #[test]
    fn command_for_key_wraps_core_keys_as_editor_commands() {
        // A non-UI key falls back to its core action, wrapped for the unified
        // dispatch path.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(FrontendCommand::Editor(Action::Save { force: false }))
        );
        assert_eq!(
            command_for_key(&km, press(KeyCode::Char('a')), PAGE),
            Some(FrontendCommand::Editor(Action::Insert("a".into())))
        );
    }

    #[test]
    fn ctrl_f_searches_this_buffer_and_the_project_search_took_the_shift_chord() {
        // The split the global-search binding was written waiting for: Ctrl+F means
        // "find in this file" nearly everywhere, so in-buffer search took it when it
        // landed and the project-wide search moved to the "find in files" chord.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('f'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(FrontendCommand::OpenFindPrompt { replacing: false })
        );
        assert_eq!(
            command_for_key(
                &km,
                with_mods(
                    KeyCode::Char('f'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                PAGE
            ),
            Some(FrontendCommand::OpenSearchPicker)
        );
        assert_eq!(
            km.shortcut_for(Command::OpenFind, Context::Editor)
                .as_deref(),
            Some("Ctrl+F")
        );
    }

    #[test]
    fn project_search_keeps_an_alt_chord_a_classic_terminal_can_reach() {
        // Ctrl+Shift+F needs the Kitty protocol's modifier reporting, so leaving it
        // as the only binding would make project search unreachable by key on a
        // classic terminal.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(&km, with_mods(KeyCode::Char('f'), KeyModifiers::ALT), PAGE),
            Some(FrontendCommand::OpenSearchPicker)
        );
        // The conventional chord is the one the palette advertises, of the two.
        assert_eq!(
            km.shortcut_for(Command::OpenSearchPicker, Context::Editor)
                .as_deref(),
            Some("Ctrl+Shift+F")
        );
    }

    #[test]
    fn f3_repeats_the_last_search() {
        // The conventional find-next key, and one classic terminals do report -
        // unlike the Ctrl+Shift chords.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(&km, press(KeyCode::F(3)), PAGE),
            Some(FrontendCommand::FindNext)
        );
        assert_eq!(
            command_for_key(&km, with_mods(KeyCode::F(3), KeyModifiers::SHIFT), PAGE),
            Some(FrontendCommand::FindPrevious)
        );
        assert_eq!(
            km.shortcut_for(Command::FindNext, Context::Editor)
                .as_deref(),
            Some("F3")
        );
    }

    #[test]
    fn a_function_key_chord_parses_and_displays_by_number() {
        assert_eq!(Chord::parse("f12").map(|c| c.code), Some(KeyCode::F(12)));
        assert_eq!(
            Chord::parse("shift+f1").map(|c| c.code),
            Some(KeyCode::F(1))
        );
        // Not a function key: `f` alone is still the letter, and `fx` is no key.
        assert_eq!(Chord::parse("f").map(|c| c.code), Some(KeyCode::Char('f')));
        assert_eq!(Chord::parse("fx"), None);
        assert_eq!(key_display(KeyCode::F(7)), "F7");
    }

    #[test]
    fn a_select_command_that_is_not_a_motion_still_parses() {
        // The `select_` prefix must not claim every name beginning with it, or every
        // future selection command that is not a movement becomes unnameable.
        assert_eq!(
            Command::parse("select_all_matches"),
            Some(Command::SelectAllMatches)
        );
        assert_eq!(
            Command::parse("select_left"),
            Some(Command::Move {
                kind: MoveKind::Left,
                extend: true,
            })
        );
        assert_eq!(Command::parse("select_nonsense"), None);
    }

    #[test]
    fn the_search_picker_command_name_parses() {
        assert_eq!(
            Command::parse("open_search_picker"),
            Some(Command::OpenSearchPicker)
        );
    }

    #[test]
    fn shortcut_for_finds_the_bound_key() {
        // Platform-independent bindings (not the OS-conditional undo/redo/clipboard).
        // Editor commands and overlay triggers are looked up the same way - one table,
        // one identity - so the palette needs no per-kind branch.
        let km = Keymap::default();
        assert_eq!(
            km.shortcut_for(Command::Save, Context::Editor).as_deref(),
            Some("Ctrl+S")
        );
        assert_eq!(
            km.shortcut_for(Command::Quit, Context::Editor).as_deref(),
            Some("Ctrl+Q")
        );
        assert_eq!(
            km.shortcut_for(Command::OpenFilePicker, Context::Editor)
                .as_deref(),
            Some("Ctrl+O")
        );
        assert_eq!(
            km.shortcut_for(Command::OpenPalette, Context::Editor)
                .as_deref(),
            Some("Ctrl+P")
        );
    }

    #[test]
    fn shortcut_for_matches_a_page_motion_without_resolving_it() {
        // Regression for the old reverse lookup, which compared *resolved* values at a
        // hardcoded page 0: a page motion resolved at any other page stopped matching
        // and its shortcut silently disappeared. Identity matching is page-free.
        let km = Keymap::default();
        assert_eq!(
            km.shortcut_for(
                Command::Move {
                    kind: MoveKind::PageDown,
                    extend: false,
                },
                Context::Editor
            )
            .as_deref(),
            Some("PageDown")
        );
    }

    #[test]
    fn shortcut_for_is_none_when_unbound() {
        // A keymap with only Save bound: everything else has no shortcut to show.
        let km = Keymap::from_pairs([("ctrl+s", "save")]).unwrap();
        assert_eq!(
            km.shortcut_for(Command::Save, Context::Editor).as_deref(),
            Some("Ctrl+S")
        );
        assert_eq!(km.shortcut_for(Command::Undo, Context::Editor), None);
        assert_eq!(
            km.shortcut_for(Command::OpenFilePicker, Context::Editor),
            None
        );
    }

    #[test]
    fn a_config_built_keymap_carries_overlay_triggers() {
        // The M5 config path is `from_pairs` and nothing else. While overlay triggers
        // lived in a second map that only `Default` filled in, a keymap built this way
        // had none - so the first user config would have silently unbound the palette
        // and the file picker, with no error to explain where they went. They are
        // ordinary commands in the one table now, so a config can bind them freely.
        let km = Keymap::from_pairs([("alt+p", "open_palette"), ("ctrl+s", "save")]).unwrap();
        assert_eq!(
            command_for_key(&km, with_mods(KeyCode::Char('p'), KeyModifiers::ALT), PAGE),
            Some(FrontendCommand::OpenPalette)
        );
        assert_eq!(
            command_for_key(
                &km,
                with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(FrontendCommand::Editor(Action::Save { force: false }))
        );
        // A misspelled overlay command is reported like any other, not dropped.
        assert_eq!(
            Keymap::from_pairs([("ctrl+k", "open_paletet")]).unwrap_err(),
            KeymapError::UnknownCommand("open_paletet".to_string())
        );
    }

    #[test]
    fn overlay_command_names_parse_and_resolve() {
        assert_eq!(Command::parse("open_palette"), Some(Command::OpenPalette));
        assert_eq!(
            Command::parse("open_file_picker"),
            Some(Command::OpenFilePicker)
        );
        // They resolve to frontend-local commands, never crossing the core seam.
        assert_eq!(
            Command::OpenPalette.resolve(PAGE),
            Some(FrontendCommand::OpenPalette)
        );
    }

    #[test]
    fn command_for_key_ignores_key_releases() {
        let km = Keymap::default();
        let mut release = press(KeyCode::Char('a'));
        release.kind = KeyEventKind::Release;
        assert_eq!(command_for_key(&km, release, PAGE), None);
    }

    /// The platform command modifier the default undo/redo bindings use: Cmd
    /// (`SUPER`) on macOS, Ctrl elsewhere - mirroring [`UNDO_REDO_BINDINGS`].
    #[cfg(target_os = "macos")]
    const CMD_MOD: KeyModifiers = KeyModifiers::SUPER;
    #[cfg(not(target_os = "macos"))]
    const CMD_MOD: KeyModifiers = KeyModifiers::CONTROL;

    #[test]
    fn platform_command_key_undoes_and_redoes() {
        // The default binds undo/redo to the OS-native command modifier (Cmd on
        // macOS, Ctrl elsewhere), so a config file needs no per-OS branch.
        assert_eq!(
            act(with_mods(KeyCode::Char('z'), CMD_MOD)),
            Some(Action::Undo)
        );
        assert_eq!(
            act(with_mods(KeyCode::Char('y'), CMD_MOD)),
            Some(Action::Redo)
        );
    }

    #[test]
    fn a_cmd_chord_parses_and_maps_when_bound() {
        // `cmd`/`super` is a first-class modifier token, so a user can bind it
        // regardless of platform (it maps to crossterm SUPER).
        let keymap = Keymap::from_pairs([("cmd+z", "undo")]).unwrap();
        assert_eq!(
            act_on(
                &keymap,
                with_mods(KeyCode::Char('z'), KeyModifiers::SUPER),
                PAGE
            ),
            Some(Action::Undo)
        );
    }

    #[test]
    fn ctrl_s_saves() {
        assert_eq!(
            act(with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(Action::Save { force: false })
        );
    }

    #[test]
    fn ctrl_shift_s_opens_the_save_prompt() {
        // Save-as is a frontend-local overlay trigger (opens the prompt line), not a
        // core action, so it resolves to a FrontendCommand and never reaches `act`.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(
                &km,
                with_mods(
                    KeyCode::Char('s'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                PAGE
            ),
            Some(FrontendCommand::OpenSavePrompt)
        );
        // Named like any other command (config-rebindable) and resolves frontend-local.
        assert_eq!(Command::parse("save_as"), Some(Command::SaveAs));
        assert_eq!(
            Command::SaveAs.resolve(PAGE),
            Some(FrontendCommand::OpenSavePrompt)
        );
        assert_eq!(
            km.shortcut_for(Command::SaveAs, Context::Editor).as_deref(),
            Some("Ctrl+Shift+S")
        );
    }

    #[test]
    fn cmd_other_char_is_unmapped_not_inserted() {
        // Regression: an unbound Cmd+<char> (e.g. Cmd+S where save is Ctrl+S) must
        // be a no-op, not insert a literal 's' via the text-entry fallback. A
        // command modifier means the char is a chord, never text.
        assert_eq!(
            act(with_mods(KeyCode::Char('s'), KeyModifiers::SUPER)),
            None
        );
    }

    #[test]
    fn ctrl_other_char_is_unmapped_not_inserted() {
        // Ctrl+a is not text and not a bound command -> no action (rather than
        // inserting a literal 'a').
        assert_eq!(
            act(with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn release_events_are_ignored() {
        // Kitty protocol reports releases; they must not re-fire the action.
        let mut ev = press(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(act(ev), None);
    }

    #[test]
    fn esc_collapses_selections_by_default() {
        // Esc reduces a multi-cursor set back to the primary (SPEC §2.2).
        assert_eq!(act(press(KeyCode::Esc)), Some(Action::CollapseSelections));
    }

    #[test]
    fn ctrl_alt_arrows_add_cursors() {
        // The column-select gesture: Ctrl+Alt+Up/Down add a cursor above/below.
        let up = with_mods(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::ALT);
        let down = with_mods(KeyCode::Down, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(act(up), Some(Action::AddCursorAbove));
        assert_eq!(act(down), Some(Action::AddCursorBelow));
    }

    #[test]
    fn multi_cursor_command_names_parse() {
        assert_eq!(
            Command::parse("add_cursor_above"),
            Some(Command::AddCursorAbove)
        );
        assert_eq!(
            Command::parse("add_cursor_below"),
            Some(Command::AddCursorBelow)
        );
        assert_eq!(
            Command::parse("collapse_selections"),
            Some(Command::CollapseSelections)
        );
    }

    #[test]
    fn chord_parses_modifiers_in_any_order_case_insensitively() {
        assert_eq!(
            Chord::parse("Ctrl+S"),
            Some(Chord {
                code: KeyCode::Char('s'),
                ctrl: true,
                shift: false,
                alt: false,
                cmd: false
            })
        );
        assert_eq!(
            Chord::parse("shift+ctrl+left"),
            Some(Chord {
                code: KeyCode::Left,
                ctrl: true,
                shift: true,
                alt: false,
                cmd: false
            })
        );
        assert_eq!(
            Chord::parse("cmd+z").map(|c| (c.cmd, c.code)),
            Some((true, KeyCode::Char('z')))
        );
        assert_eq!(
            Chord::parse("super+z").map(|c| c.cmd),
            Some(true) // `super` is an alias for the command modifier
        );
        assert_eq!(
            Chord::parse("pageup").map(|c| c.code),
            Some(KeyCode::PageUp)
        );
        assert_eq!(Chord::parse("nonsense"), None);
        assert_eq!(Chord::parse("ctrl+"), None); // modifiers with no key
    }

    #[test]
    fn command_parses_names_including_move_and_select_variants() {
        assert_eq!(Command::parse("save"), Some(Command::Save));
        assert_eq!(
            Command::parse("move_line_start"),
            Some(Command::Move {
                kind: MoveKind::LineStart,
                extend: false
            })
        );
        assert_eq!(
            Command::parse("select_page_down"),
            Some(Command::Move {
                kind: MoveKind::PageDown,
                extend: true
            })
        );
        assert_eq!(Command::parse("frobnicate"), None);
    }

    #[test]
    fn from_pairs_reports_bad_chord_and_command() {
        assert_eq!(
            Keymap::from_pairs([("ctrl+nope", "save")]).unwrap_err(),
            KeymapError::UnknownChord("ctrl+nope".to_string())
        );
        assert_eq!(
            Keymap::from_pairs([("ctrl+s", "explode")]).unwrap_err(),
            KeymapError::UnknownCommand("explode".to_string())
        );
    }

    #[test]
    fn a_custom_binding_overrides_a_default_chord() {
        // The config path: build a keymap from user pairs and confirm the rebind
        // takes effect - here Esc (unbound by default) becomes Quit.
        let keymap = Keymap::from_pairs([("esc", "quit")]).unwrap();
        assert_eq!(
            act_on(&keymap, press(KeyCode::Esc), PAGE),
            Some(Action::Quit)
        );
    }

    // --- Contexts (M9 Stage 2, SPEC §10.5) ------------------------------------

    /// A user config's `[keys]` table, layered over the built-ins the way
    /// `config::parse` does it.
    fn layered(toml_text: &str) -> (Keymap, Vec<KeymapError>) {
        let mut keymap = Keymap::default();
        let table: toml::Table = toml::from_str(toml_text).expect("test table parses");
        let rejected = keymap.extend_from_table(&table);
        (keymap, rejected)
    }

    #[test]
    fn a_subtable_binds_in_that_context_and_nowhere_else() {
        // The shape a config writes: flat rows are the editor's, a subtable is a
        // context. A picker binding must not become an editor one.
        let (km, rejected) =
            layered("\"ctrl+n\" = \"quit\"\n[picker]\n\"ctrl+n\" = \"next_item\"\n");
        assert!(rejected.is_empty(), "{rejected:?}");
        let ctrl_n = with_mods(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(km.bound(Context::Picker, ctrl_n), Some(Command::NextItem));
        assert_eq!(km.bound(Context::Editor, ctrl_n), Some(Command::Quit));
        assert_eq!(km.bound(Context::Prompt, ctrl_n), None);
    }

    #[test]
    fn a_command_written_outside_its_scope_is_reported_rather_than_applied() {
        // A binding that parses, applies and never fires is the silent failure SPEC
        // §8 forbids - so the scope is checked where the row is read.
        let (km, rejected) = layered("\"ctrl+n\" = \"next_item\"\n");
        assert_eq!(
            rejected,
            vec![KeymapError::WrongContext {
                command: "next_item".to_string(),
                context: Context::Editor,
            }]
        );
        assert_eq!(
            km.bound(
                Context::Editor,
                with_mods(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            None,
            "the bad row bound nothing"
        );
        // And the other way round: an editor command in a surface table.
        let (_, rejected) = layered("[confirm]\n\"ctrl+s\" = \"save\"\n");
        assert_eq!(
            rejected,
            vec![KeymapError::WrongContext {
                command: "save".to_string(),
                context: Context::Confirm,
            }]
        );
        assert_eq!(
            rejected[0].to_string(),
            "command `save` cannot be bound in the `confirm` context"
        );
    }

    #[test]
    fn the_context_set_is_closed_and_a_bad_row_costs_only_itself() {
        // An unknown subtable is a typo, not a table for a surface that does not exist
        // yet; and reporting it must not cost the rows around it.
        let (km, rejected) =
            layered("[pickr]\n\"ctrl+n\" = \"next_item\"\n[picker]\n\"ctrl+j\" = \"next_item\"\n");
        assert_eq!(
            rejected,
            vec![KeymapError::UnknownContext("pickr".to_string())]
        );
        assert_eq!(rejected[0].to_string(), "unknown key context `pickr`");
        assert_eq!(
            km.bound(
                Context::Picker,
                with_mods(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            Some(Command::NextItem),
            "the good table still applied"
        );
    }

    #[test]
    fn contexts_do_not_nest_and_a_value_that_is_not_a_name_is_reported() {
        let (_, rejected) = layered("[picker.inner]\n\"ctrl+n\" = \"next_item\"\n");
        assert_eq!(
            rejected,
            vec![KeymapError::NotABinding {
                chord: "inner".to_string(),
                kind: "table",
            }]
        );
        assert_eq!(
            rejected[0].to_string(),
            "`inner` is bound to a table, not a command name"
        );
        // The same at the top level, where a number is no more a command than a table.
        let (_, rejected) = layered("\"ctrl+n\" = 3\n");
        assert_eq!(
            rejected,
            vec![KeymapError::NotABinding {
                chord: "ctrl+n".to_string(),
                kind: "integer",
            }]
        );
    }

    #[test]
    fn nop_is_bindable_in_every_context_and_nothing_else_is_universal() {
        for context in [
            Context::Editor,
            Context::Macos,
            Context::Picker,
            Context::Prompt,
            Context::Find,
            Context::Confirm,
            Context::Replace,
        ] {
            assert!(Command::Nop.allowed_in(context), "{}", context.name());
        }
        // `cancel` is every *surface*, and no platform or editor table.
        assert!(!Command::Cancel.allowed_in(Context::Editor));
        assert!(!Command::Cancel.allowed_in(Context::Macos));
        assert!(Command::Cancel.allowed_in(Context::Picker));
        // `delete_backward` is the shared name: one intent over whatever has focus.
        for context in [
            Context::Editor,
            Context::Picker,
            Context::Prompt,
            Context::Find,
        ] {
            assert!(
                Command::DeleteBackward.allowed_in(context),
                "{}",
                context.name()
            );
        }
        assert!(!Command::DeleteBackward.allowed_in(Context::Confirm));
    }

    #[test]
    fn a_platform_table_holds_editor_commands_and_only_editor_commands() {
        // It sits in the *editor's* stack rather than over a surface, so it answers to
        // the editor's scope - which is what lets `keys.toml`'s own `[macos]` row exist.
        let (_, rejected) = layered("[macos]\n\"ctrl+k\" = \"save\"\n");
        assert!(rejected.is_empty(), "{rejected:?}");
        let (_, rejected) = layered("[linux]\n\"ctrl+k\" = \"accept\"\n");
        assert_eq!(
            rejected,
            vec![KeymapError::WrongContext {
                command: "accept".to_string(),
                context: Context::Linux,
            }]
        );
    }

    #[test]
    fn the_platform_context_wins_over_the_editors_on_the_same_chord() {
        // Top down through the editor's stack: the platform row shadows the editor's.
        // Written against `Context::PLATFORM` so it asserts the same thing everywhere.
        let table = format!("[{}]\n\"ctrl+s\" = \"quit\"\n", Context::PLATFORM.name());
        let (km, rejected) = layered(&table);
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(
            act_on(
                &km,
                with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL),
                PAGE
            ),
            Some(Action::Quit),
            "the platform row shadowed `save`"
        );
    }

    #[test]
    fn a_surface_context_never_answers_for_the_editor() {
        // The one thing `bound` must not do: a picker's Enter is not the editor's.
        let km = Keymap::default();
        assert_eq!(
            command_for_key(&km, press(KeyCode::Enter), PAGE),
            Some(FrontendCommand::Editor(Action::Insert("\n".into()))),
            "Enter is still a newline in the buffer"
        );
        assert_eq!(
            km.bound(Context::Picker, press(KeyCode::Enter)),
            Some(Command::Accept)
        );
    }

    #[test]
    fn shortcut_for_answers_in_the_context_it_is_asked_about() {
        // The display rule's whole mechanism: the walk's question renders `Y` from the
        // `replace` context, and the palette never sees it in the editor's.
        let km = Keymap::default();
        assert_eq!(
            km.shortcut_for(Command::ReplaceYes, Context::Replace)
                .as_deref(),
            Some("Y")
        );
        assert_eq!(km.shortcut_for(Command::ReplaceYes, Context::Editor), None);
        assert_eq!(
            km.shortcut_for(Command::ConfirmYes, Context::Confirm)
                .as_deref(),
            Some("Y")
        );
        // A rebind moves what is displayed, which is the point of rendering it.
        let (rebound, _) = layered("[replace]\n\"ctrl+y\" = \"replace_yes\"\n\"y\" = \"nop\"\n");
        assert_eq!(
            rebound
                .shortcut_for(Command::ReplaceYes, Context::Replace)
                .as_deref(),
            Some("Ctrl+Y")
        );
    }

    #[test]
    fn the_editor_context_shows_a_chord_only_the_platform_table_binds() {
        // The platform is part of the editor's own stack rather than a surface, so a
        // command bound only there still has a chord to show. Off a Mac `quit` has two
        // bindings and the deterministic pick is Ctrl+Q either way.
        let table = format!(
            "[{}]\n\"ctrl+alt+k\" = \"toggle_scrollbar\"\n",
            Context::PLATFORM.name()
        );
        let (km, _) = layered(&table);
        assert_eq!(
            km.shortcut_for(Command::ToggleScrollbar, Context::Editor)
                .as_deref(),
            Some("Ctrl+Alt+K")
        );
    }

    #[test]
    fn a_shifted_letter_is_one_chord_however_the_terminal_reports_it() {
        // Kitty sends `Y` *and* SHIFT; a classic terminal sends `Y` alone. Both have to
        // reach the same row, or a binding works in one terminal and not the other.
        let km = Keymap::from_pairs([("shift+y", "quit"), ("y", "save")]).unwrap();
        for reported in [
            press(KeyCode::Char('Y')),
            with_mods(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            with_mods(KeyCode::Char('y'), KeyModifiers::SHIFT),
        ] {
            assert_eq!(
                act_on(&km, reported, PAGE),
                Some(Action::Quit),
                "{reported:?}"
            );
        }
        // The unshifted key is still its own row.
        assert_eq!(
            act_on(&km, press(KeyCode::Char('y')), PAGE),
            Some(Action::Save { force: false })
        );
        // And an *unbound* upper-case letter still types its own case: the fold is a
        // lookup concern, not a typing one.
        assert_eq!(
            act(press(KeyCode::Char('Q'))),
            Some(Action::Insert("Q".into()))
        );
    }
}

#[cfg(test)]
mod readme_tests {
    use super::{Chord, Command, Keymap};

    /// Backticked tokens in the README's key section that the *keymap* does not own,
    /// each for a reason rather than because it was inconvenient:
    ///
    /// - `Alt+Click` is a mouse gesture. A drag has no chord, which is why gestures
    ///   are deliberately not data (SPEC §10.5) - but leaving it out of the table
    ///   would hide a real binding from the reader.
    /// - `Shift` and `mod` are bare modifiers named in prose ("hold `Shift` to
    ///   select", "`mod` is whichever key the platform commands with"), not chords.
    ///
    /// The query-replace walk's `y`/`n`/`a`/`q` **left this list** at M9 Stage 2: they
    /// are rows in the `replace` context now, so the README is held to them like every
    /// other chord. This list shrinking is what a surface becoming bindable looks
    /// like from here.
    const NOT_CHORDS: &[&str] = &["Alt+Click", "Shift", "mod"];

    /// A document cannot query a keymap, so the README's key table stays literal -
    /// and is held to the default keymap by this test instead, the same device the
    /// theme and config formats already use for their worked examples.
    ///
    /// It is the drift that motivated M9: the help had advertised `Ctrl+F  Search the
    /// project` since M7 moved project search off that chord, and nothing failed
    /// because nothing connected the sentence to the binding. The help renders its
    /// chords now; the README is checked.
    #[test]
    fn every_chord_the_readme_advertises_is_bound() {
        let readme = include_str!("../../../README.md");
        let section = readme
            .split("\n## Keys\n")
            .nth(1)
            .and_then(|rest| rest.split("\n## ").next())
            .expect("README documents the keys in a `## Keys` section");
        let keymap = Keymap::default();
        let mut checked = 0;
        // Every backticked token in the section, not just the table's first column.
        // The prose around the table names chords too - `Ctrl+G` sits in a row's
        // explanation, and the clipboard is described entirely below the table - and a
        // scan that stopped at the first cell would have left exactly those to drift.
        for token in section.split('`').skip(1).step_by(2) {
            if NOT_CHORDS.contains(&token) {
                continue;
            }
            let chord = Chord::parse(token)
                .unwrap_or_else(|| panic!("README names `{token}`, which is not a chord"));
            // In **any** context, since the README describes surfaces as well as the
            // buffer: `y` is a real binding, just not one the editor context holds.
            // Which context a chord belongs to is a claim the prose makes and this
            // test cannot check, so it checks the part it can - that the chord does
            // something somewhere.
            assert!(
                keymap
                    .bindings
                    .iter()
                    .any(|((_, bound), command)| *bound == chord && *command != Command::Nop),
                "README advertises `{token}`, which the default keymap does not bind"
            );
            checked += 1;
        }
        // The section names well over two dozen chords; a slicing bug that read none of
        // them would otherwise pass this test by finding nothing to disagree with.
        assert!(checked > 25, "only {checked} chords read from the section");
    }
}
