//! Frontend configuration - the seam where user settings enter the TUI.
//!
//! Today every value is a hardcoded [`Default`]; **no file is read yet**. The point
//! of this module is to give file-loaded config a single place to land: M5 adds
//! `serde` + `toml` (SPEC §3 "Config" row, §10.5) and a `Config::load(path)` that
//! deserializes the user's config file, falling back to these defaults for anything
//! unset. Everything downstream already reads from a [`Config`] value, so that
//! change touches only this module.
//!
//! Scope is deliberately frontend-only: styling (a [`Theme`]) and, next, the keymap
//! (key→intent is frontend-owned per SPEC §2.2/§12.2). The core stays config-free -
//! chrome and key bindings never cross the seam.

use ratatui::style::{Color, Modifier, Style};

use crate::keymap::Keymap;

/// All user-configurable frontend settings, resolved once at startup and threaded
/// into the render and input paths. Grows as configurable surfaces land, so it is
/// passed as a whole rather than field-by-field (SPEC §10.5).
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Colors/attributes for the non-text chrome.
    pub theme: Theme,
    /// Key -> intent bindings (`Default` is the built-in map; a config file's
    /// `[keymap]` table will replace it via [`Keymap::from_pairs`]).
    pub keymap: Keymap,
}

/// Chrome styling for the frontend's non-text UI: the head/status bars and the
/// line-number gutter. Bundled into one value (not scattered `const`s) so a config
/// can swap it wholesale. `Copy` - each [`Style`] is `Copy` - so threading it per
/// frame is free and it never touches the render hot path beyond a field read.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Top bar: buffer name (left) and line count (right).
    pub head_bar: Style,
    /// Bottom bar: cursor position (left) and buffer metrics (right).
    pub status_bar: Style,
    /// Gutter line numbers away from the cursor - dimmed so they recede.
    pub gutter: Style,
    /// The cursor line's gutter number - brightened/bold so the active row stands out.
    pub gutter_current: Style,
    /// Selected text. Uses explicit RGB (not named ANSI colors, which the terminal
    /// remaps to its own palette and can render as low-contrast light-on-light):
    /// a muted dark blue behind true white keeps a legible contrast on any theme.
    /// Once syntax coloring lands (M4) this may soften to let those foregrounds
    /// show through.
    pub selection: Style,
    /// The cursor line's background - a subtle tint filling the whole row so the
    /// active line is easy to find without pulling the eye like a selection does.
    pub current_line: Style,
    /// The marker for a *secondary* (non-primary) caret in a multi-cursor set
    /// (SPEC §2.2). The terminal has a single real cursor, which the primary caret
    /// uses; the others are painted as a one-cell reversed block so they are visible.
    pub secondary_cursor: Style,
    /// The prompt-line overlay (SPEC §7.5): a bottom-row single-line input, e.g. the
    /// file-open prompt. Styled distinctly from the status bar it covers so an open
    /// prompt reads as a mode rather than just another transient message.
    pub prompt: Style,
}

impl Default for Theme {
    /// The built-in theme. This is exactly what `Config::load` will fall back to for
    /// any field the user's config file leaves unset, so the defaults live here and
    /// nowhere else.
    fn default() -> Self {
        Self {
            head_bar: Style::new()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            status_bar: Style::new().fg(Color::Black).bg(Color::Gray),
            gutter: Style::new().fg(Color::DarkGray),
            gutter_current: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            selection: Style::new()
                .bg(Color::Rgb(38, 79, 120))
                .fg(Color::Rgb(255, 255, 255)),
            current_line: Style::new().bg(Color::Indexed(236)),
            // Reversed video reads as a block caret against whatever it sits on,
            // without committing to a palette color (SPEC §2.2 multi-cursor).
            secondary_cursor: Style::new().add_modifier(Modifier::REVERSED),
            // The same accent blue as a selection, filling the row, so an open
            // prompt is unmistakably a distinct mode over the editor (SPEC §7.5).
            prompt: Style::new()
                .fg(Color::Rgb(255, 255, 255))
                .bg(Color::Rgb(38, 79, 120)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_carries_the_builtin_theme() {
        let config = Config::default();
        assert_eq!(config.theme.head_bar.bg, Some(Color::Gray));
        assert_eq!(config.theme.status_bar.bg, Some(Color::Gray));
        assert_eq!(config.theme.head_bar.fg, Some(Color::Black));
    }

    #[test]
    fn default_config_carries_a_working_keymap() {
        use crate::keymap::action_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use vortex_core::Action;

        let config = Config::default();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            action_for_key(&config.keymap, ctrl_s, 10),
            Some(Action::Save)
        );
    }

    #[test]
    fn default_theme_matches_the_builtin_palette() {
        let t = Theme::default();
        assert_eq!(t.gutter.fg, Some(Color::DarkGray));
        assert_eq!(t.gutter_current.fg, Some(Color::White));
        assert!(t.gutter_current.add_modifier.contains(Modifier::BOLD));
        assert!(t.head_bar.add_modifier.contains(Modifier::BOLD));
        // Selection pairs an explicit-RGB background with a contrasting foreground
        // so selected text stays legible on any terminal palette; the current-line
        // tint is a background-only wash.
        assert_eq!(t.selection.bg, Some(Color::Rgb(38, 79, 120)));
        assert_eq!(t.selection.fg, Some(Color::Rgb(255, 255, 255)));
        assert_eq!(t.current_line.bg, Some(Color::Indexed(236)));
        // A secondary caret is a reversed-video marker (multi-cursor, SPEC §2.2).
        assert!(t.secondary_cursor.add_modifier.contains(Modifier::REVERSED));
    }
}
