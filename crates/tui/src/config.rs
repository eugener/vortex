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

/// All user-configurable frontend settings, resolved once at startup and threaded
/// into the render path. Grows as configurable surfaces land - the keymap is the
/// next field (SPEC §10.5) - so it is passed as a whole rather than field-by-field.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Colors/attributes for the non-text chrome.
    pub theme: Theme,
    // keymap: Keymap  - M1+, once the key→intent table is data rather than code.
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
    fn default_theme_matches_the_builtin_palette() {
        let t = Theme::default();
        assert_eq!(t.gutter.fg, Some(Color::DarkGray));
        assert_eq!(t.gutter_current.fg, Some(Color::White));
        assert!(t.gutter_current.add_modifier.contains(Modifier::BOLD));
        assert!(t.head_bar.add_modifier.contains(Modifier::BOLD));
    }
}
