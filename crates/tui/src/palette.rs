//! The command palette (SPEC §7.5) - a [`Picker`] over a curated set of named
//! commands, opened with Ctrl+P.
//!
//! This is one instance of the shared fuzzy [`Picker`]; the file picker
//! ([`crate::filepicker`]) is the other. The palette just supplies the item list:
//! the discrete named commands a user picks by name, deliberately excluding motions
//! and text entry (those are not palette-worthy). A pick runs through the identical
//! [`Command`] dispatch a bound key uses.

use crate::compositor::Layer;
use crate::config::Theme;
// The keymap's bindable-command identity, as distinct from the dispatchable
// `crate::command::Command` the picker items carry.
use crate::keymap::{Command as Bindable, Keymap};
use crate::picker::{Item, Picker};

/// The curated command set the palette lists, in display order, each named by the
/// same [`Bindable`] identity the keymap binds - so a row's label, its shortcut, and
/// what it runs all come from one entry instead of three parallel lists.
///
/// Motions and text entry are deliberately absent: they are not palette-worthy.
const PALETTE: &[(&str, Bindable)] = &[
    ("Find File…", Bindable::OpenFilePicker),
    ("Change Theme…", Bindable::OpenThemePicker),
    ("Save File", Bindable::Save),
    ("Undo", Bindable::Undo),
    ("Redo", Bindable::Redo),
    ("Copy", Bindable::Copy),
    ("Cut", Bindable::Cut),
    ("Paste", Bindable::Paste),
    ("Add Cursor Above", Bindable::AddCursorAbove),
    ("Add Cursor Below", Bindable::AddCursorBelow),
    ("Collapse Selections", Bindable::CollapseSelections),
    ("Quit", Bindable::Quit),
];

/// The page size palette entries resolve against. Every listed command is
/// page-independent (no motions are palette-worthy), so the value is never observed;
/// `page_independent` in this module's tests holds [`PALETTE`] to that.
const PALETTE_PAGE: usize = 0;

/// Build the palette's items. Each entry's shortcut is looked up from the keymap by
/// command identity (single source of truth), so it stays right even after a rebind.
fn registry(keymap: &Keymap) -> Vec<Item> {
    PALETTE
        .iter()
        .map(|&(label, bound)| Item {
            label: label.to_string(),
            shortcut: keymap.shortcut_for(bound),
            command: bound.resolve(PALETTE_PAGE),
        })
        .collect()
}

/// Open the command palette, styled from the theme, with shortcuts from the keymap.
pub fn open(theme: &Theme, keymap: &Keymap) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Commands",
        registry(keymap),
        false,
        theme.palette,
        theme.palette_selected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::command::Command;
    use vortex_core::Action;

    #[test]
    fn registry_lists_the_named_commands_with_shortcuts() {
        let items = registry(&Keymap::default());
        // A representative core command and the file-picker opener are present, and
        // their shortcuts are populated from the keymap.
        let save = items
            .iter()
            .find(|i| matches!(i.command, Command::Editor(Action::Save)))
            .expect("Save File listed");
        assert_eq!(save.shortcut.as_deref(), Some("Ctrl+S"));
        let find = items
            .iter()
            .find(|i| i.command == Command::OpenFilePicker)
            .expect("Find File listed");
        assert_eq!(find.shortcut.as_deref(), Some("Ctrl+O"));
        // No duplicate labels (they are the fuzzy haystacks).
        for (n, item) in items.iter().enumerate() {
            assert!(
                !items[n + 1..].iter().any(|o| o.label == item.label),
                "duplicate label: {}",
                item.label
            );
        }
    }

    #[test]
    fn every_palette_command_is_page_independent() {
        // The palette resolves its entries against a fixed PALETTE_PAGE because none
        // of them is a page motion. Adding e.g. `select_page_down` to the list would
        // silently bake in a page size of 0 (a motion that goes nowhere), so hold the
        // table to the property the constant assumes rather than to a comment.
        for &(label, bound) in PALETTE {
            assert_eq!(
                bound.resolve(0),
                bound.resolve(99),
                "{label} depends on the page size and is not palette-worthy"
            );
        }
    }

    #[test]
    fn palette_shortcuts_track_a_rebind() {
        // The point of looking shortcuts up by command identity: a user config that
        // moves Save to another chord must be reflected in the palette, with no
        // second table to update.
        let rebound = Keymap::from_pairs([("ctrl+w", "save")]).unwrap();
        let items = registry(&rebound);
        let save = items
            .iter()
            .find(|i| matches!(i.command, Command::Editor(Action::Save)))
            .expect("Save File listed");
        assert_eq!(save.shortcut.as_deref(), Some("Ctrl+W"));
        // A command the config left unbound simply shows no shortcut.
        let quit = items
            .iter()
            .find(|i| matches!(i.command, Command::Editor(Action::Quit)))
            .expect("Quit listed");
        assert_eq!(quit.shortcut, None);
    }
}
