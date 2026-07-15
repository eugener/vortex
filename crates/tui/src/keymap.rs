//! Key -> `Action` translation (SPEC §1, §12.2).
//!
//! Key->intent mapping is **frontend-owned**: the core only ever sees intent
//! (`Action`), never keystrokes. A future GUI maps its own keys to the same
//! actions. This is a pure function of a key event so it is unit-testable without
//! a terminal (SPEC §13) - the raw `event::read` loop in `main` is the only
//! untestable part.
//!
//! M1's map is deliberately minimal (motion + text edit + quit); the configurable
//! keymap *file format* (modal vs modeless, chords) is its own design, drafted
//! alongside the full `Action` vocabulary (SPEC §11 "Keymap configuration").

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use vortex_core::{Action, Motion};

/// Translate a key event into an `Action`, or `None` if the key is unmapped.
///
/// Only key **press** (and repeat) events map to actions; releases are ignored so
/// the Kitty protocol's release reporting (SPEC §9) does not double-fire edits.
/// `Shift` on a motion key produces the `extend` variant (grow the selection).
pub fn action_for_key(key: KeyEvent) -> Option<Action> {
    // With the Kitty protocol enabled we receive Release events too; act only on
    // Press/Repeat. (Classic terminals only ever send Press, so this is safe.)
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let extend = key.modifiers.contains(KeyModifiers::SHIFT);

    let motion = |m: Motion| Some(Action::MoveCursor { motion: m, extend });

    match key.code {
        // Ctrl+Q / Ctrl+C: quit.
        KeyCode::Char('q' | 'c') if ctrl => Some(Action::Quit),

        // Text entry. A Ctrl-modified char is a command, not text, so it is not
        // inserted here (only the mappings above consume it).
        KeyCode::Char(c) if !ctrl => Some(Action::Insert(c.to_string())),
        KeyCode::Enter => Some(Action::Insert("\n".to_string())),
        KeyCode::Tab => Some(Action::Insert("\t".to_string())),

        // Deletion.
        KeyCode::Backspace => Some(Action::DeleteBackward),
        KeyCode::Delete => Some(Action::DeleteForward),

        // Motion (Shift = extend selection).
        KeyCode::Left => motion(Motion::Left),
        KeyCode::Right => motion(Motion::Right),
        KeyCode::Up => motion(Motion::Up),
        KeyCode::Down => motion(Motion::Down),
        KeyCode::Home => motion(Motion::LineStart),
        KeyCode::End => motion(Motion::LineEnd),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_char_inserts() {
        assert_eq!(
            action_for_key(press(KeyCode::Char('a'))),
            Some(Action::Insert("a".into()))
        );
    }

    #[test]
    fn enter_and_tab_insert_whitespace() {
        assert_eq!(
            action_for_key(press(KeyCode::Enter)),
            Some(Action::Insert("\n".into()))
        );
        assert_eq!(
            action_for_key(press(KeyCode::Tab)),
            Some(Action::Insert("\t".into()))
        );
    }

    #[test]
    fn backspace_and_delete() {
        assert_eq!(
            action_for_key(press(KeyCode::Backspace)),
            Some(Action::DeleteBackward)
        );
        assert_eq!(
            action_for_key(press(KeyCode::Delete)),
            Some(Action::DeleteForward)
        );
    }

    #[test]
    fn arrows_map_to_motions_without_extend() {
        assert_eq!(
            action_for_key(press(KeyCode::Left)),
            Some(Action::MoveCursor {
                motion: Motion::Left,
                extend: false
            })
        );
        assert_eq!(
            action_for_key(press(KeyCode::Up)),
            Some(Action::MoveCursor {
                motion: Motion::Up,
                extend: false
            })
        );
    }

    #[test]
    fn shift_arrow_extends() {
        assert_eq!(
            action_for_key(with_mods(KeyCode::Right, KeyModifiers::SHIFT)),
            Some(Action::MoveCursor {
                motion: Motion::Right,
                extend: true
            })
        );
    }

    #[test]
    fn home_end_map_to_line_bounds() {
        assert_eq!(
            action_for_key(press(KeyCode::Home)),
            Some(Action::MoveCursor {
                motion: Motion::LineStart,
                extend: false
            })
        );
        assert_eq!(
            action_for_key(press(KeyCode::End)),
            Some(Action::MoveCursor {
                motion: Motion::LineEnd,
                extend: false
            })
        );
    }

    #[test]
    fn ctrl_q_and_ctrl_c_quit() {
        assert_eq!(
            action_for_key(with_mods(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            action_for_key(with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }

    #[test]
    fn ctrl_other_char_is_unmapped_not_inserted() {
        // Ctrl+a is not text and not a mapped command in M1 -> no action (rather
        // than inserting a literal 'a').
        assert_eq!(
            action_for_key(with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn release_events_are_ignored() {
        // Kitty protocol reports releases; they must not re-fire the action.
        let mut ev = press(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(action_for_key(ev), None);
    }

    #[test]
    fn esc_is_unmapped_in_m1() {
        assert_eq!(action_for_key(press(KeyCode::Esc)), None);
    }
}
