//! Pickers for a file's on-disk form: its encoding and its line terminator
//! (SPEC §7.5's picker surface over SPEC §10.1's format).
//!
//! Two instances of the shared fuzzy [`Picker`], like the palette and the file
//! picker. What is worth saying is *why* they exist: the encoding of a file with no
//! BOM is a guess (`file.rs` samples for UTF-8 and falls back to windows-1252), and
//! a guess needs a way to be corrected. Reaching them by clicking the status bar's
//! own readout is the whole point - the place that tells you the answer is the place
//! you change it.
//!
//! Neither picker previews. A theme preview is a local repaint, but changing the
//! encoding crosses the seam and would mean a round trip per highlight move - and
//! there is nothing to see until the file is written anyway.

use crate::command::Command;
use crate::compositor::Layer;
use crate::config::Config;
use crate::picker::{Item, Picker};
use vortex_core::{Action, LineEnding, file::OFFERED_ENCODINGS};

/// The encodings a save can use, opening on the one in use so the picker says what
/// the file currently *is* as well as what it could be.
pub fn encoding(config: &Config, current: &str) -> Box<dyn Layer> {
    let items = OFFERED_ENCODINGS
        .iter()
        .map(|&name| Item {
            label: name.to_string(),
            dim_columns: 0,
            // The current encoding is marked rather than reordered: a list that
            // moves under you is harder to learn than one that does not.
            shortcut: (name == current).then(|| "current".to_string()),
            command: Command::Editor(Action::SetEncoding(name.to_string())),
        })
        .collect();
    let selected = OFFERED_ENCODINGS.iter().position(|&n| n == current);
    let picker = Picker::new("Encoding", items, false, config);
    Box::new(match selected {
        Some(index) => picker.with_selected(index),
        // An encoding the list does not offer - nothing stops a file being loaded
        // as one - so the picker opens at the top rather than pretending.
        None => picker,
    })
}

/// The two line terminators (SPEC §10.1), named the way the status bar names them.
pub fn line_ending(config: &Config, current: LineEnding) -> Box<dyn Layer> {
    let choices = [LineEnding::Lf, LineEnding::Crlf];
    let items = choices
        .iter()
        .map(|&eol| Item {
            label: format!("{} ({})", eol.name(), description(eol)),
            dim_columns: 0,
            shortcut: (eol == current).then(|| "current".to_string()),
            command: Command::Editor(Action::SetLineEnding(eol)),
        })
        .collect();
    let selected = choices.iter().position(|&eol| eol == current).unwrap_or(0);
    Box::new(Picker::new("Line endings", items, false, config).with_selected(selected))
}

/// What a terminator is called outside a status bar, so the row means something to
/// someone who has not memorized the abbreviations.
fn description(eol: LineEnding) -> &'static str {
    match eol {
        LineEnding::Lf => "Unix",
        LineEnding::Crlf => "Windows",
    }
}

#[cfg(test)]
#[path = "formatpicker_tests.rs"]
mod tests;
