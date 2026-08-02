use super::*;
use crate::compositor::send;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// What the picker commits when Enter is pressed on the row it opened on.
fn commit_on_open(mut layer: Box<dyn Layer>) -> Vec<Command> {
    send(&mut *layer, press(KeyCode::Enter));
    layer.take_commands()
}

#[test]
fn the_encoding_picker_opens_on_the_one_in_use() {
    // Opening on the current encoding is what makes the picker a readout as well as
    // a chooser - and it means Enter is a no-op rather than a surprise.
    let layer = encoding(&Theme::default(), "Shift_JIS");
    assert_eq!(
        commit_on_open(layer),
        vec![Command::Editor(Action::SetEncoding("Shift_JIS".into()))]
    );
}

#[test]
fn an_encoding_the_list_does_not_offer_opens_at_the_top() {
    // A file can be loaded as an encoding the curated list leaves out; the picker
    // must still open, rather than pretend that encoding is selected.
    let layer = encoding(&Theme::default(), "x-mac-cyrillic");
    assert_eq!(
        commit_on_open(layer),
        vec![Command::Editor(Action::SetEncoding(
            OFFERED_ENCODINGS[0].into()
        ))]
    );
}

#[test]
fn every_offered_encoding_is_one_the_core_accepts() {
    // The list is hand-curated, so a typo in it would offer a row that fails when
    // picked. Held to the core's own resolution rather than to a second copy.
    let format = vortex_core::FileFormat::default();
    for &name in OFFERED_ENCODINGS {
        assert!(
            format.with_encoding(name).is_ok(),
            "{name} is offered but not resolvable"
        );
    }
}

#[test]
fn the_offered_encodings_are_named_the_way_the_status_bar_names_them() {
    // The picker marks the current row by comparing labels against
    // `FileFormat::encoding_name`, so a label that is a valid *alias* but not the
    // canonical name would never match and the current row would never be marked.
    let format = vortex_core::FileFormat::default();
    for &name in OFFERED_ENCODINGS {
        let resolved = format.with_encoding(name).unwrap();
        assert_eq!(resolved.encoding_name(), name, "{name} is not canonical");
    }
}

#[test]
fn the_line_ending_picker_offers_both_and_opens_on_the_current_one() {
    let layer = line_ending(&Theme::default(), LineEnding::Crlf);
    assert_eq!(
        commit_on_open(layer),
        vec![Command::Editor(Action::SetLineEnding(LineEnding::Crlf))]
    );
    let layer = line_ending(&Theme::default(), LineEnding::Lf);
    assert_eq!(
        commit_on_open(layer),
        vec![Command::Editor(Action::SetLineEnding(LineEnding::Lf))]
    );
}

#[test]
fn moving_off_the_current_row_picks_the_other_terminator() {
    let mut layer = line_ending(&Theme::default(), LineEnding::Lf);
    send(&mut *layer, press(KeyCode::Down));
    send(&mut *layer, press(KeyCode::Enter));
    assert_eq!(
        layer.take_commands(),
        vec![Command::Editor(Action::SetLineEnding(LineEnding::Crlf))]
    );
}

#[test]
fn neither_picker_previews() {
    // A preview would cross the seam on every highlight move, and there is nothing
    // to see until the file is written.
    let mut layer = encoding(&Theme::default(), "UTF-8");
    send(&mut *layer, press(KeyCode::Down));
    assert!(layer.take_commands().is_empty());
    let mut layer = line_ending(&Theme::default(), LineEnding::Lf);
    send(&mut *layer, press(KeyCode::Down));
    assert!(layer.take_commands().is_empty());
}

#[test]
fn escaping_either_picker_commits_nothing() {
    for mut layer in [
        encoding(&Theme::default(), "UTF-8"),
        line_ending(&Theme::default(), LineEnding::Lf),
    ] {
        send(&mut *layer, press(KeyCode::Esc));
        assert!(layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }
}
