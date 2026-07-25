//! Property test: opening a file and saving it back changes nothing (SPEC §10.1, §13).
//!
//! The whole point of remembering a file's encoding and line terminator is that a
//! save reproduces the file. The unit tests in `file.rs` pin the interesting
//! *cases*; this pins the *invariant* over arbitrary input, so a future change to
//! detection or transcoding cannot quietly start rewriting user files.
//!
//! Two things are deliberately outside the property, because for them "unchanged"
//! is the wrong expectation, not an unmet one:
//!
//! - **A leading BOM in an otherwise-arbitrary byte string.** Those bytes claim an
//!   encoding the rest of the file does not honor, and a lossy decode is the honest
//!   answer (`file.rs` covers it as a case).
//! - **Mixed line endings.** A file that is 90% CRLF is a CRLF file, so its stray
//!   lone `\n` is normalized on the way out. That is the documented behavior, and
//!   the second property below pins the consistent-terminator case that must hold.

use proptest::prelude::*;
use vortex_core::file::load;

/// The BOMs `Encoding::for_bom` recognizes - see the module note for why generated
/// input that starts with one is excluded.
fn starts_with_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xEF, 0xBB, 0xBF])
        || bytes.starts_with(&[0xFF, 0xFE])
        || bytes.starts_with(&[0xFE, 0xFF])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Any byte string - valid UTF-8 or not - survives a load/save round trip.
    /// This is what lets a file in an encoding we cannot name still be opened:
    /// editing one line of it must not corrupt the other 400.
    #[test]
    fn arbitrary_bytes_survive_a_load_and_save(
        bytes in prop::collection::vec(any::<u8>(), 0..2048)
            .prop_filter("BOM and CR are covered by their own cases", |b| {
                !starts_with_bom(b) && !b.contains(&b'\r')
            })
    ) {
        let (text, format) = load(&bytes);
        let written = format.encode(&text, false).expect("what loaded must re-encode");
        prop_assert_eq!(written, bytes);
    }

    /// A file with one consistent terminator keeps it, whichever it is: the lines
    /// are normalized to LF in the buffer and converted back on the way out.
    #[test]
    fn a_consistent_line_terminator_is_preserved(
        lines in prop::collection::vec("[a-z é語]{0,8}", 0..12),
        crlf in any::<bool>(),
        trailing in any::<bool>(),
    ) {
        let eol = if crlf { "\r\n" } else { "\n" };
        let mut file = lines.join(eol);
        if trailing && !file.is_empty() {
            file.push_str(eol);
        }

        let (text, format) = load(file.as_bytes());
        prop_assert!(!text.contains('\r'), "the buffer is always LF: {text:?}");
        let written = format.encode(&text, false).expect("what loaded must re-encode");
        prop_assert_eq!(String::from_utf8(written).unwrap(), file);
    }
}
