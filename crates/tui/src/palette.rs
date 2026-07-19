//! The command palette (SPEC §7.5) - a [`Picker`] over a curated set of named
//! commands, opened with Ctrl+P.
//!
//! This is one instance of the shared fuzzy [`Picker`]; the file picker
//! ([`crate::filepicker`]) is the other. The palette just supplies the item list:
//! the discrete named commands a user picks by name, deliberately excluding motions
//! and text entry (those are not palette-worthy). A pick runs through the identical
//! [`Command`] dispatch a bound key uses.

use vortex_core::Action;

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Theme;
use crate::picker::{Item, Picker};

/// The curated command set the palette lists.
fn registry() -> Vec<Item> {
    let e = |label: &str, command| Item {
        label: label.to_string(),
        command,
    };
    vec![
        e("Find File…", Command::OpenFilePicker),
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

/// Open the command palette, styled from the theme.
pub fn open(theme: &Theme) -> Box<dyn Layer> {
    Box::new(Picker::new(
        "Commands",
        registry(),
        false,
        theme.palette,
        theme.palette_selected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_the_named_commands() {
        let items = registry();
        // A representative core command and the file-picker opener are present.
        assert!(
            items
                .iter()
                .any(|i| matches!(i.command, Command::Editor(Action::Save)))
        );
        assert!(items.iter().any(|i| i.command == Command::OpenFilePicker));
        // No duplicate labels (they are the fuzzy haystacks).
        for (n, item) in items.iter().enumerate() {
            assert!(
                !items[n + 1..].iter().any(|o| o.label == item.label),
                "duplicate label: {}",
                item.label
            );
        }
    }
}
