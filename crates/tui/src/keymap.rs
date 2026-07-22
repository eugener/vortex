//! Key -> `Action` translation, table-driven so it can be user-configured (SPEC
//! §1, §2.2, §10.5, §12.2).
//!
//! Key->intent mapping is **frontend-owned**: the core only ever sees intent
//! (`Action`), never keystrokes. A future GUI maps its own keys to the same actions.
//!
//! The map is **data, not code**: a [`Keymap`] is a set of `(chord -> command)`
//! bindings, and [`command_for_key`] is a pure lookup over it. Both sides of a binding
//! parse from strings ([`Chord::parse`], [`Command::parse`]) - the built-in
//! [`Keymap::default`] is itself built from a table of `("ctrl+s", "save")`-shaped
//! string pairs, so the default bindings are expressed in the *exact* form a config
//! file will use. That is the config seam: **no file is read yet**; M5 adds `toml`
//! parsing (SPEC §3) and calls [`Keymap::from_pairs`] with the user's table, falling
//! back to these defaults. Everything is a pure function of a key event, so it stays
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
    fn from_event(key: &KeyEvent) -> Self {
        Self {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            cmd: key.modifiers.contains(KeyModifiers::SUPER),
        }
    }

    /// Parse a chord string such as `"ctrl+shift+left"`, `"cmd+z"`, `"s"`, or
    /// `"pageup"`. Modifier tokens (`ctrl`/`control`, `shift`, `alt`/`opt`,
    /// `cmd`/`super`/`win`) may appear in any order before the key; matching is
    /// case-insensitive. Returns `None` if the key token is unknown. (A literal `+`
    /// key is not yet expressible - a known limit.)
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
        one if one.chars().count() == 1 => KeyCode::Char(one.chars().next()?),
        _ => return None,
    })
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
        if let Some(kind) = name.strip_prefix("move_") {
            return parse_move_kind(kind).map(|kind| Command::Move {
                kind,
                extend: false,
            });
        }
        if let Some(kind) = name.strip_prefix("select_") {
            return parse_move_kind(kind).map(|kind| Command::Move { kind, extend: true });
        }
        Some(match name {
            "quit" => Command::Quit,
            "save" => Command::Save,
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
            _ => return None,
        })
    }

    /// Finalize into the dispatchable command for this frame (`page` sizes page
    /// motions). Overlay triggers resolve to a frontend-local command; everything
    /// else wraps a core [`Action`] for the seam.
    pub fn resolve(self, page: usize) -> FrontendCommand {
        let action = match self {
            Command::OpenPalette => return FrontendCommand::OpenPalette,
            Command::OpenFilePicker => return FrontendCommand::OpenFilePicker,
            Command::OpenThemePicker => return FrontendCommand::OpenThemePicker,
            Command::Quit => Action::Quit,
            Command::Save => Action::Save,
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
        FrontendCommand::Editor(action)
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

/// The built-in bindings, in the same `(chord, command)` string form a config file
/// uses. `extend` is explicit: each motion has a plain and a `shift+`/`select_` pair.
/// Text entry (printable chars) is a fallback in [`command_for_key`], not listed here.
const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("ctrl+q", "quit"),
    ("ctrl+s", "save"),
    ("enter", "insert_newline"),
    ("tab", "insert_tab"),
    ("backspace", "delete_backward"),
    ("delete", "delete_forward"),
    ("left", "move_left"),
    ("right", "move_right"),
    ("up", "move_up"),
    ("down", "move_down"),
    ("home", "move_line_start"),
    ("end", "move_line_end"),
    ("pageup", "move_page_up"),
    ("pagedown", "move_page_down"),
    ("shift+left", "select_left"),
    ("shift+right", "select_right"),
    ("shift+up", "select_up"),
    ("shift+down", "select_down"),
    ("shift+home", "select_line_start"),
    ("shift+end", "select_line_end"),
    ("shift+pageup", "select_page_up"),
    ("shift+pagedown", "select_page_down"),
    // Multi-cursor (SPEC §2.2). The Ctrl+Alt+Arrow chords need the Kitty protocol's
    // modifier reporting (negotiated at startup); a classic terminal simply never
    // matches them rather than misfiring. Esc collapses back to one cursor.
    ("ctrl+alt+up", "add_cursor_above"),
    ("ctrl+alt+down", "add_cursor_below"),
    ("esc", "collapse_selections"),
    // Overlay triggers (SPEC §7.5). They live in this same table, named like any
    // other command, so a user config can rebind them - and so `from_pairs` alone
    // yields a complete keymap. Ctrl+O is the primary "open": the fuzzy file picker.
    ("ctrl+o", "open_file_picker"),
    ("ctrl+p", "open_palette"),
    ("ctrl+t", "open_theme_picker"),
];

/// Bindings on the platform's native command modifier: Cmd on macOS (crossterm
/// `SUPER`), Ctrl elsewhere. Kept separate from [`DEFAULT_BINDINGS`] so only these
/// are OS-conditional; the rest of the map is identical everywhere. On macOS the
/// Cmd chords are delivered only by Kitty-protocol terminals (which report Cmd) - a
/// documented trade-off for matching each OS's muscle memory. Raw mode delivers the
/// modified letters as key events, never a suspend/flow signal, so binding them is
/// safe.
///
/// Clipboard follows each OS's convention: Cmd+C/X/V on macOS, Ctrl+C/X/V elsewhere.
/// This reclaims Ctrl+C from quit on non-mac (quit stays Ctrl+Q); on macOS Ctrl+C
/// remains quit (see [`MACOS_ONLY_BINDINGS`]) since copy is Cmd+C there.
#[cfg(target_os = "macos")]
const COMMAND_MOD_BINDINGS: &[(&str, &str)] = &[
    ("cmd+z", "undo"),
    ("cmd+y", "redo"),
    ("cmd+c", "copy"),
    ("cmd+x", "cut"),
    ("cmd+v", "paste"),
];
#[cfg(not(target_os = "macos"))]
const COMMAND_MOD_BINDINGS: &[(&str, &str)] = &[
    ("ctrl+z", "undo"),
    ("ctrl+y", "redo"),
    ("ctrl+c", "copy"),
    ("ctrl+x", "cut"),
    ("ctrl+v", "paste"),
];

/// Bindings that exist only on macOS: there, Ctrl+C is free (copy is Cmd+C), so it
/// keeps its terminal-conventional meaning of quit alongside Ctrl+Q. Empty on other
/// platforms, where Ctrl+C is copy (see [`COMMAND_MOD_BINDINGS`]) and quit is Ctrl+Q.
#[cfg(target_os = "macos")]
const MACOS_ONLY_BINDINGS: &[(&str, &str)] = &[("ctrl+c", "quit")];
#[cfg(not(target_os = "macos"))]
const MACOS_ONLY_BINDINGS: &[(&str, &str)] = &[];

/// The resolved key bindings. Opaque so its representation can change (e.g. gain
/// per-mode maps) without touching call sites; built via [`Keymap::from_pairs`].
///
/// One table for every binding, edit and overlay alike: a second map would be a
/// second thing `from_pairs` has to remember to populate, and the M5 config path
/// goes through `from_pairs` only - so anything it missed would silently vanish the
/// first time a user wrote a config file.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Chord, Command>,
}

impl Keymap {
    /// Build a keymap from `(chord, command)` string pairs - the shape a config
    /// file's `[keymap]` table deserializes to. Later pairs override earlier ones on
    /// the same chord (so a user table layered after the defaults wins).
    ///
    /// # Errors
    /// Returns [`KeymapError`] naming the first unparseable chord or command, so a
    /// bad config line is surfaced to the user rather than silently dropped (SPEC §8).
    pub fn from_pairs<'a, I>(pairs: I) -> Result<Self, KeymapError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut bindings = HashMap::new();
        for (chord, command) in pairs {
            let chord_key =
                Chord::parse(chord).ok_or_else(|| KeymapError::UnknownChord(chord.to_string()))?;
            let command = Command::parse(command)
                .ok_or_else(|| KeymapError::UnknownCommand(command.to_string()))?;
            bindings.insert(chord_key, command);
        }
        Ok(Self { bindings })
    }

    /// The shortcut bound to `command`, formatted for display (e.g. `"Ctrl+S"`), or
    /// `None` if it is unbound. Lets the palette show each command's key without a
    /// second source of truth - a rebind (M5 config) keeps the palette correct.
    ///
    /// Matched on the command **identity**, so the lookup is exact: comparing
    /// *resolved* values instead would need a page size to resolve against, and any
    /// command carrying runtime data would silently stop matching (no error - the
    /// shortcut would just stop appearing).
    ///
    /// A command may have several bindings (on macOS Quit is both Ctrl+Q and Ctrl+C);
    /// `max` picks one **deterministically** - HashMap order is not stable - and
    /// happens to prefer Ctrl+Q over Ctrl+C.
    pub fn shortcut_for(&self, command: Command) -> Option<String> {
        self.bindings
            .iter()
            .filter(|(_, bound)| **bound == command)
            .map(|(chord, _)| chord.display())
            .max()
    }
}

impl Default for Keymap {
    /// The built-in keymap: the OS-independent [`DEFAULT_BINDINGS`] plus the
    /// platform's [`COMMAND_MOD_BINDINGS`] and [`MACOS_ONLY_BINDINGS`]. Parsing
    /// cannot fail - all three tables are compile-time constants covered by a test -
    /// so the `expect` is invariant-proven. Nothing is added after `from_pairs`, so
    /// the defaults are reachable by exactly the path a config file takes.
    fn default() -> Self {
        let pairs = DEFAULT_BINDINGS
            .iter()
            .chain(COMMAND_MOD_BINDINGS.iter())
            .chain(MACOS_ONLY_BINDINGS.iter())
            .copied();
        Self::from_pairs(pairs).expect("built-in default bindings must be valid")
    }
}

/// A binding that failed to parse, naming the offending token so the user can fix
/// their config. Carries no source location yet (M5 adds line context on file load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    UnknownChord(String),
    UnknownCommand(String),
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::UnknownChord(s) => write!(f, "unknown key chord `{s}`"),
            KeymapError::UnknownCommand(s) => write!(f, "unknown command `{s}`"),
        }
    }
}

impl std::error::Error for KeymapError {}

/// Translate a key event into the [`FrontendCommand`] the event loop dispatches
/// (SPEC §7.5), or `None` if the key is unmapped.
///
/// One lookup for every binding, edit and overlay alike - the routing decision lives
/// in the command a chord names, not in which table it was found in. Only key
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

    let chord = Chord::from_event(&key);
    if let Some(command) = keymap.bindings.get(&chord) {
        return Some(command.resolve(page));
    }

    // Text-entry fallback: an unbound printable char inserts itself. A Ctrl- or
    // Cmd-modified char is a command chord, never text, so it is not inserted -
    // otherwise an unbound Cmd+S / Ctrl+A would type a literal `s`/`a`. (Alt is
    // deliberately allowed through: on many layouts Alt/Option composes accented
    // characters that are legitimate text.)
    if !chord.ctrl
        && !chord.cmd
        && let KeyCode::Char(c) = key.code
    {
        return Some(FrontendCommand::Editor(Action::Insert(c.to_string())));
    }

    None
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
        // Guards the `expect` in `Keymap::default` - proves DEFAULT_BINDINGS parses.
        let _ = Keymap::default();
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
            km.shortcut_for(Command::OpenThemePicker).as_deref(),
            Some("Ctrl+T")
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
            Some(FrontendCommand::Editor(Action::Save))
        );
        assert_eq!(
            command_for_key(&km, press(KeyCode::Char('a')), PAGE),
            Some(FrontendCommand::Editor(Action::Insert("a".into())))
        );
    }

    #[test]
    fn shortcut_for_finds_the_bound_key() {
        // Platform-independent bindings (not the OS-conditional undo/redo/clipboard).
        // Editor commands and overlay triggers are looked up the same way - one table,
        // one identity - so the palette needs no per-kind branch.
        let km = Keymap::default();
        assert_eq!(km.shortcut_for(Command::Save).as_deref(), Some("Ctrl+S"));
        assert_eq!(km.shortcut_for(Command::Quit).as_deref(), Some("Ctrl+Q"));
        assert_eq!(
            km.shortcut_for(Command::OpenFilePicker).as_deref(),
            Some("Ctrl+O")
        );
        assert_eq!(
            km.shortcut_for(Command::OpenPalette).as_deref(),
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
            km.shortcut_for(Command::Move {
                kind: MoveKind::PageDown,
                extend: false,
            })
            .as_deref(),
            Some("PageDown")
        );
    }

    #[test]
    fn shortcut_for_is_none_when_unbound() {
        // A keymap with only Save bound: everything else has no shortcut to show.
        let km = Keymap::from_pairs([("ctrl+s", "save")]).unwrap();
        assert_eq!(km.shortcut_for(Command::Save).as_deref(), Some("Ctrl+S"));
        assert_eq!(km.shortcut_for(Command::Undo), None);
        assert_eq!(km.shortcut_for(Command::OpenFilePicker), None);
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
            Some(FrontendCommand::Editor(Action::Save))
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
            FrontendCommand::OpenPalette
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
            Some(Action::Save)
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
}
