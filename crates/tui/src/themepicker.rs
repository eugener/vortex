//! The theme picker (SPEC §7.5) - a [`Picker`] over every theme [`crate::theme`]
//! discovers, opened with Ctrl+T.
//!
//! Like the palette and the file picker it only supplies the item list; unlike them
//! it **previews**. Moving the highlight applies that theme immediately (the whole
//! point: you cannot judge a color scheme from its name), Enter keeps it, and Esc
//! restores the one you opened with. That is possible without any core round-trip
//! because chrome is entirely frontend-owned - a preview is a local repaint, not an
//! edit, so the seam never hears about it.
//!
//! The list opens on the theme in use, and a theme that lives in the user's config
//! directory says so in the shortcut column - which is how you tell a shadowed
//! built-in from the original.

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::picker::{Item, Picker};
use crate::theme::{self, Origin};

/// Marker shown where the other pickers show a shortcut: this theme came from the
/// user's themes directory rather than being one of the built-ins.
const USER_THEME: &str = "user";

/// Build one row per discovered theme, in discovery (name) order.
fn registry(entries: Vec<theme::Entry>) -> Vec<Item> {
    entries
        .into_iter()
        .map(|entry| Item {
            shortcut: matches!(entry.origin, Origin::User(_)).then(|| USER_THEME.to_string()),
            command: Command::SetTheme(entry.name.clone()),
            label: entry.name,
        })
        .collect()
}

/// Open the theme picker, highlighting `current` and previewing as you move.
pub fn open(theme: &Theme, current: &str) -> Box<dyn Layer> {
    let entries = theme::discover();
    // Restoring means naming the theme in use; if it is somehow not in the list
    // (a user file deleted while the editor ran), the highlight falls on the first
    // row and Esc still restores by name - the load then fails loudly in a toast
    // rather than leaving the preview silently applied.
    let selected = entries.iter().position(|e| e.name == current).unwrap_or(0);
    Box::new(
        Picker::new(
            "Themes",
            registry(entries),
            false,
            theme.palette,
            theme.palette_selected,
        )
        .with_selected(selected)
        .previewing(Command::SetTheme(current.to_string())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::EventResult;
    use crate::testutil::TempDir;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn theme_name(command: &Command) -> &str {
        match command {
            Command::SetTheme(name) => name,
            other => panic!("expected a theme command, got {other:?}"),
        }
    }

    #[test]
    fn every_discovered_theme_is_listed_with_its_origin() {
        let dir = TempDir::new();
        dir.file("midnight.toml", "");
        let mut entries = theme::discover();
        entries.push(theme::Entry {
            name: "midnight".into(),
            origin: Origin::User(dir.path.join("midnight.toml")),
        });
        let items = registry(entries);

        let builtin = items.iter().find(|i| i.label == theme::DEFAULT).unwrap();
        assert_eq!(builtin.shortcut, None, "a built-in is unmarked");
        assert_eq!(builtin.command, Command::SetTheme(theme::DEFAULT.into()));
        let user = items.iter().find(|i| i.label == "midnight").unwrap();
        assert_eq!(user.shortcut.as_deref(), Some(USER_THEME));
    }

    #[test]
    fn it_opens_on_the_theme_in_use_and_previews_nothing() {
        // Opening the picker must not change what you are looking at.
        let mut picker = open(&Theme::default(), "phosphor");
        assert!(picker.take_commands().is_empty());
        // The highlight is on the current theme, so committing straight away is a
        // no-op rather than a jump to whatever sorted first.
        picker.handle_key(press(KeyCode::Enter));
        let committed = picker.take_commands();
        assert_eq!(theme_name(&committed[0]), "phosphor");
    }

    #[test]
    fn moving_previews_and_escaping_restores() {
        let mut picker = open(&Theme::default(), theme::DEFAULT);
        assert_eq!(picker.handle_key(press(KeyCode::Up)), EventResult::Consumed);
        let previewed = picker.take_commands();
        assert_eq!(previewed.len(), 1, "moving previews exactly one theme");
        assert_ne!(
            theme_name(&previewed[0]),
            theme::DEFAULT,
            "the highlight moved off the theme in use"
        );

        picker.handle_key(press(KeyCode::Esc));
        assert!(picker.is_finished());
        let restored = picker.take_commands();
        assert_eq!(
            theme_name(&restored[0]),
            theme::DEFAULT,
            "Esc puts back the theme the picker opened with"
        );
    }

    #[test]
    fn a_filter_that_moves_the_highlight_previews_too() {
        // Preview follows the highlight, however it moved - typing re-ranks the list
        // under it, which is a move even though no arrow key was pressed.
        let mut picker = open(&Theme::default(), theme::DEFAULT);
        for c in "phos".chars() {
            picker.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let previewed = picker.take_commands();
        assert_eq!(
            theme_name(previewed.last().unwrap()),
            "phosphor",
            "the top match is previewed"
        );
    }
}
