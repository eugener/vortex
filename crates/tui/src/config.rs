//! Frontend configuration - the seam where user settings enter the TUI.
//!
//! Styling is the first setting to become a real file: a [`Theme`] is loaded from a
//! TOML file by [`crate::theme`], and the theme picker swaps one in at runtime. The
//! keymap is still a hardcoded [`Default`]; M5 adds `Config::load(path)` reading the
//! user's config file over the same `toml` seam (SPEC §3 "Config" row, §10.5) for
//! the rest. Everything downstream already reads from a [`Config`] value, so that
//! change touches only this module.
//!
//! Scope is deliberately frontend-only: styling and the keymap (key→intent is
//! frontend-owned per SPEC §2.2/§12.2). The core stays config-free - chrome and key
//! bindings never cross the seam.

use ratatui::style::{Color, Modifier, Style};

use crate::keymap::Keymap;

/// All user-configurable frontend settings, resolved once at startup and threaded
/// into the render and input paths. Grows as configurable surfaces land, so it is
/// passed as a whole rather than field-by-field (SPEC §10.5).
#[derive(Debug, Clone)]
pub struct Config {
    /// Colors/attributes for the non-text chrome.
    pub theme: Theme,
    /// Which theme [`Self::theme`] came from, so the picker can highlight the one
    /// in use and restore it when a preview is cancelled.
    pub theme_name: String,
    /// Key -> intent bindings (`Default` is the built-in map; a config file's
    /// `[keymap]` table will replace it via [`Keymap::from_pairs`]).
    pub keymap: Keymap,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            theme_name: crate::theme::DEFAULT.to_string(),
            keymap: Keymap::default(),
        }
    }
}

/// Chrome styling for the frontend's non-text UI: the head/status bars and the
/// line-number gutter. Bundled into one value (not scattered `const`s) so a config
/// can swap it wholesale. `Copy` - each [`Style`] is `Copy` - so threading it per
/// frame is free and it never touches the render hot path beyond a field read.
///
/// Every field here is a key in a theme file ([`crate::theme`]); adding one means
/// adding it there too, and the round-trip test in that module holds the built-in
/// default and `themes/undertow.toml` to being the same theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The editor body's own ground: the background the text area is filled with
    /// and the foreground unstyled text takes. Painted as the base style beneath
    /// every row, so a theme is not at the mercy of the user's terminal background
    /// (a light theme in a black terminal would otherwise be unreadable).
    pub text: Style,
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
    /// Informational toasts (SPEC §7.5): file opened/saved. Calm, so they inform
    /// without alarming.
    pub toast_info: Style,
    /// Error toasts: save failed, edit rejected. High-contrast red so a failure is
    /// unmistakable (SPEC §8: a failure must be visible, never silent).
    pub toast_error: Style,
    /// The command palette box (SPEC §7.5): its border, query row, and unselected
    /// entries.
    pub palette: Style,
    /// The palette's highlighted row - an accent fill so the selection is obvious.
    pub palette_selected: Style,
    /// The four LSP diagnostic severities (SPEC §5). The `fg` colors the underline
    /// under a flagged span and the mark in the gutter; a background, if set, is
    /// ignored for the underline (which paints only the foreground) so a theme need
    /// not reserve one. Kept as four fields rather than a lookup so a theme file
    /// names each severity explicitly, the same as every other slot.
    pub diagnostic_error: Style,
    pub diagnostic_warning: Style,
    pub diagnostic_information: Style,
    pub diagnostic_hint: Style,
    /// Syntax highlighting colors (SPEC §5, M4). The core's ~18
    /// [`HighlightKind`](vortex_core::HighlightKind)s are painted from these eight
    /// roles (see [`Theme::highlight`]) rather than one field per kind: a coherent
    /// scheme is a handful of hues, not eighteen, and a theme file names eight keys
    /// instead of eighteen. Only the `fg` is used - a highlight colors the glyph and
    /// lets selection and current-line backgrounds show through.
    pub syntax_keyword: Style,
    pub syntax_function: Style,
    pub syntax_type: Style,
    pub syntax_string: Style,
    pub syntax_comment: Style,
    pub syntax_constant: Style,
    pub syntax_variable: Style,
    pub syntax_punctuation: Style,
}

impl Theme {
    /// The style for a diagnostic [`Severity`](vortex_core::Severity) - the seam's
    /// semantic tag resolved to concrete colors here, in the frontend, exactly as
    /// SPEC §5 requires (the core never names a color).
    pub fn diagnostic(&self, severity: vortex_core::Severity) -> Style {
        use vortex_core::Severity;
        match severity {
            Severity::Error => self.diagnostic_error,
            Severity::Warning => self.diagnostic_warning,
            Severity::Information => self.diagnostic_information,
            Severity::Hint => self.diagnostic_hint,
        }
    }

    /// The style for a syntax [`HighlightKind`](vortex_core::HighlightKind), the
    /// M4 twin of [`Theme::diagnostic`]: the core's semantic tag resolved to a color
    /// here. Related kinds collapse to one role (a macro and a call are both
    /// `syntax_function`, a builtin type and a user type both `syntax_type`) so the
    /// palette stays small. `HighlightKind` is `non_exhaustive`, so an unknown future
    /// kind falls back to the body text color - visible, unstyled, never a panic.
    pub fn highlight(&self, kind: vortex_core::HighlightKind) -> Style {
        use vortex_core::HighlightKind as K;
        match kind {
            K::Keyword => self.syntax_keyword,
            K::Function | K::Macro | K::Constructor => self.syntax_function,
            K::Type | K::TypeBuiltin => self.syntax_type,
            K::String | K::Escape => self.syntax_string,
            K::Comment => self.syntax_comment,
            K::Constant | K::ConstantBuiltin => self.syntax_constant,
            K::Variable | K::Parameter | K::Property => self.syntax_variable,
            K::Attribute | K::Label | K::Operator | K::Punctuation => self.syntax_punctuation,
            _ => self.text,
        }
    }
}

impl Default for Theme {
    /// The built-in theme: **undertow**, the house dark scheme (see
    /// `themes/undertow.toml`, whose every value this mirrors).
    ///
    /// Written out in Rust rather than parsed from that file at startup so the
    /// editor can never fail to have a theme - `Theme::default()` is infallible, and
    /// `theme::the_default_theme_is_the_undertow_file` is what keeps the two in
    /// step. It is also the fallback for any slot a loaded theme file leaves unset.
    fn default() -> Self {
        // Depth is carried by blue: each surface that comes forward gets a lighter,
        // bluer ground. Colors are explicit RGB, never named ANSI ones, which the
        // terminal remaps to its own palette and can render as low-contrast
        // light-on-light (the same reason `theme::color` accepts hex only).
        Self {
            text: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x15, 0x18, 0x23)),
            head_bar: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x11, 0x14, 0x1d))
                .add_modifier(Modifier::BOLD),
            status_bar: Style::new()
                .fg(Color::Rgb(0x8a, 0x93, 0xb5))
                .bg(Color::Rgb(0x11, 0x14, 0x1d)),
            gutter: Style::new().fg(Color::Rgb(0x4a, 0x52, 0x73)),
            gutter_current: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .add_modifier(Modifier::BOLD),
            selection: Style::new()
                .fg(Color::Rgb(0xee, 0xf1, 0xfa))
                .bg(Color::Rgb(0x2b, 0x35, 0x57)),
            current_line: Style::new().bg(Color::Rgb(0x1c, 0x20, 0x31)),
            // A violet block: the terminal has one real cursor, which the primary
            // caret uses, so the others need a color of their own (SPEC §2.2).
            secondary_cursor: Style::new()
                .fg(Color::Rgb(0x15, 0x18, 0x23))
                .bg(Color::Rgb(0x7d, 0x6c, 0xe0)),
            // Toasts (SPEC §7.5): a sunk slate for info, a strong red for errors, so
            // a failure is unmistakable (SPEC §8: never silent).
            toast_info: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x22, 0x28, 0x3c)),
            toast_error: Style::new()
                .fg(Color::Rgb(0xff, 0xe7, 0xec))
                .bg(Color::Rgb(0x7a, 0x2f, 0x3d))
                .add_modifier(Modifier::BOLD),
            // The palette floats above the body, so it gets its own lighter panel;
            // the selection's blue marks the highlighted row (SPEC §7.5).
            palette: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x1a, 0x1e, 0x2c)),
            palette_selected: Style::new()
                .fg(Color::Rgb(0xee, 0xf1, 0xfa))
                .bg(Color::Rgb(0x2b, 0x35, 0x57))
                .add_modifier(Modifier::BOLD),
            // Diagnostics (SPEC §5): a red error and an amber warning carry the
            // usual severity signal, while information and hint stay quiet - a
            // desaturated blue and a muted grey - so a wall of hints never shouts
            // over a real error. These are the underline/gutter foregrounds.
            diagnostic_error: Style::new().fg(Color::Rgb(0xe0, 0x6c, 0x75)),
            diagnostic_warning: Style::new().fg(Color::Rgb(0xd6, 0x9d, 0x53)),
            diagnostic_information: Style::new().fg(Color::Rgb(0x61, 0x9a, 0xd6)),
            diagnostic_hint: Style::new().fg(Color::Rgb(0x7d, 0x86, 0xa8)),
            // Syntax (SPEC §5, M4): a restrained scheme on undertow's blue ground -
            // a violet keyword, a blue function, a warm-gold type, a green string, a
            // dim slate comment, an orange constant, the body color for variables so
            // ordinary identifiers stay calm, and a muted punctuation.
            syntax_keyword: Style::new().fg(Color::Rgb(0xb1, 0x8b, 0xe0)),
            syntax_function: Style::new().fg(Color::Rgb(0x61, 0x9a, 0xd6)),
            syntax_type: Style::new().fg(Color::Rgb(0xd6, 0xb2, 0x70)),
            syntax_string: Style::new().fg(Color::Rgb(0x8c, 0xc2, 0x65)),
            syntax_comment: Style::new()
                .fg(Color::Rgb(0x5a, 0x63, 0x82))
                .add_modifier(Modifier::ITALIC),
            syntax_constant: Style::new().fg(Color::Rgb(0xd6, 0x9d, 0x53)),
            syntax_variable: Style::new().fg(Color::Rgb(0xcc, 0xd2, 0xe4)),
            syntax_punctuation: Style::new().fg(Color::Rgb(0x8a, 0x93, 0xb5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_carries_the_builtin_theme_and_its_name() {
        let config = Config::default();
        assert_eq!(config.theme, Theme::default());
        // The name must be a theme that actually resolves, or the picker opens with
        // nothing highlighted and a cancelled preview restores a theme that is gone.
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
        assert_eq!(
            crate::theme::load_named(&config.theme_name).unwrap(),
            config.theme
        );
    }

    #[test]
    fn default_config_carries_a_working_keymap() {
        use crate::command::Command;
        use crate::keymap::command_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use vortex_core::Action;

        let config = Config::default();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_s, 10),
            Some(Command::Editor(Action::Save))
        );
        // Overlay triggers ride the same table, so the resolved config carries them
        // too - the property that breaks if they are ever built outside `from_pairs`.
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_p, 10),
            Some(Command::OpenPalette)
        );
    }

    #[test]
    fn default_theme_pins_every_color_to_true_color() {
        // Named/indexed ANSI colors are remapped by the user's terminal profile, so
        // a theme built from them cannot promise the contrast it was designed with.
        // Every slot the built-in theme fills must therefore be `Color::Rgb`.
        let t = Theme::default();
        let slots = [
            ("text", t.text),
            ("head_bar", t.head_bar),
            ("status_bar", t.status_bar),
            ("gutter", t.gutter),
            ("gutter_current", t.gutter_current),
            ("selection", t.selection),
            ("current_line", t.current_line),
            ("secondary_cursor", t.secondary_cursor),
            ("toast_info", t.toast_info),
            ("toast_error", t.toast_error),
            ("palette", t.palette),
            ("palette_selected", t.palette_selected),
            ("diagnostic_error", t.diagnostic_error),
            ("diagnostic_warning", t.diagnostic_warning),
            ("diagnostic_information", t.diagnostic_information),
            ("diagnostic_hint", t.diagnostic_hint),
            ("syntax_keyword", t.syntax_keyword),
            ("syntax_function", t.syntax_function),
            ("syntax_type", t.syntax_type),
            ("syntax_string", t.syntax_string),
            ("syntax_comment", t.syntax_comment),
            ("syntax_constant", t.syntax_constant),
            ("syntax_variable", t.syntax_variable),
            ("syntax_punctuation", t.syntax_punctuation),
        ];
        for (name, style) in slots {
            for color in [style.fg, style.bg].into_iter().flatten() {
                assert!(matches!(color, Color::Rgb(..)), "{name}: {color:?}");
            }
        }
        // The body has both a ground and an ink, so the theme owns the whole surface.
        assert!(t.text.fg.is_some() && t.text.bg.is_some());
        assert!(t.gutter_current.add_modifier.contains(Modifier::BOLD));
        assert!(t.head_bar.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn highlight_maps_every_kind_to_its_role() {
        use vortex_core::HighlightKind as K;
        let t = Theme::default();
        // A representative kind from each of the eight roles resolves to that role's
        // slot; related kinds share a role (a macro paints as a function).
        assert_eq!(t.highlight(K::Keyword), t.syntax_keyword);
        assert_eq!(t.highlight(K::Function), t.syntax_function);
        assert_eq!(t.highlight(K::Macro), t.syntax_function);
        assert_eq!(t.highlight(K::Type), t.syntax_type);
        assert_eq!(t.highlight(K::TypeBuiltin), t.syntax_type);
        assert_eq!(t.highlight(K::String), t.syntax_string);
        assert_eq!(t.highlight(K::Escape), t.syntax_string);
        assert_eq!(t.highlight(K::Comment), t.syntax_comment);
        assert_eq!(t.highlight(K::ConstantBuiltin), t.syntax_constant);
        assert_eq!(t.highlight(K::Parameter), t.syntax_variable);
        assert_eq!(t.highlight(K::Punctuation), t.syntax_punctuation);
        assert_eq!(t.highlight(K::Attribute), t.syntax_punctuation);
    }
}
