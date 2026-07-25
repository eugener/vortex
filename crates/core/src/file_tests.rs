use super::*;

use encoding_rs::SHIFT_JIS;

/// `load` then `encode` with the default final-newline policy off - the shape
/// almost every test here asserts on, since preservation *is* the contract.
fn roundtrip(bytes: &[u8]) -> Vec<u8> {
    let Loaded { text, format, .. } = load(bytes);
    format.encode(&text, false).expect("re-encodes")
}

#[test]
fn plain_utf8_lf_file_loads_and_roundtrips() {
    let Loaded { text, format, .. } = load(b"one\ntwo\n");
    assert_eq!(text, "one\ntwo\n");
    assert_eq!(format.encoding_name(), "UTF-8");
    assert_eq!(format.eol, LineEnding::Lf);
    assert!(!format.bom);
    assert_eq!(roundtrip(b"one\ntwo\n"), b"one\ntwo\n");
}

#[test]
fn empty_file_is_utf8_lf_and_stays_empty() {
    let Loaded { text, format, .. } = load(b"");
    assert_eq!(text, "");
    assert_eq!(format, FileFormat::default());
    // Even with the final-newline policy on: "ensure" must not mean "create".
    assert_eq!(format.encode("", true).unwrap(), b"");
}

#[test]
fn crlf_is_normalized_on_load_and_restored_on_save() {
    let Loaded { text, format, .. } = load(b"one\r\ntwo\r\n");
    // The rope only ever holds LF, so every motion and column rule sees one shape.
    assert_eq!(text, "one\ntwo\n");
    assert_eq!(format.eol, LineEnding::Crlf);
    assert_eq!(format.encode(&text, false).unwrap(), b"one\r\ntwo\r\n");
}

#[test]
fn dominant_line_ending_wins_a_mixed_file() {
    // Two CRLF against one LF: the file is a CRLF file with a stray line.
    assert_eq!(load(b"a\r\nb\nc\r\n").format.eol, LineEnding::Crlf);
    // ...and the other way, which is the tie-break case too (1 vs 1 -> LF).
    assert_eq!(load(b"a\r\nb\nc\n").format.eol, LineEnding::Lf);
    assert_eq!(load(b"no newlines at all").format.eol, LineEnding::Lf);
}

#[test]
fn saving_a_crlf_file_does_not_double_the_carriage_returns() {
    // Text pasted into a CRLF buffer can carry its own `\r\n`. Rewriting bare
    // `\n` blindly would turn those into `\r\r\n`.
    let format = FileFormat {
        encoding: UTF_8,
        bom: false,
        eol: LineEnding::Crlf,
    };
    assert_eq!(format.encode("a\r\nb\n", false).unwrap(), b"a\r\nb\r\n");
}

#[test]
fn utf8_bom_is_preserved() {
    let bytes = b"\xEF\xBB\xBFhello";
    let Loaded { text, format, .. } = load(bytes);
    assert_eq!(text, "hello"); // the BOM is not editable content
    assert_eq!(format.encoding_name(), "UTF-8");
    assert!(format.bom);
    assert_eq!(roundtrip(bytes), bytes);
}

#[test]
fn utf16le_file_roundtrips_byte_for_byte() {
    // `encoding_rs` cannot encode UTF-16 - it would write UTF-8 and call it done.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "hi\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let Loaded { text, format, .. } = load(&bytes);
    assert_eq!(text, "hi\n");
    assert_eq!(format.encoding_name(), "UTF-16LE");
    assert_eq!(roundtrip(&bytes), bytes);
}

#[test]
fn utf16be_file_roundtrips_byte_for_byte() {
    let mut bytes = vec![0xFE, 0xFF];
    for unit in "hi\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    let Loaded { text, format, .. } = load(&bytes);
    assert_eq!(text, "hi\n");
    assert_eq!(format.encoding_name(), "UTF-16BE");
    assert_eq!(roundtrip(&bytes), bytes);
}

#[test]
fn utf16_crlf_survives_both_conversions_at_once() {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "a\r\nb".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let Loaded { text, format, .. } = load(&bytes);
    assert_eq!(text, "a\nb");
    assert_eq!(format.eol, LineEnding::Crlf);
    assert_eq!(roundtrip(&bytes), bytes);
}

#[test]
fn an_unrecognized_encoding_falls_back_to_windows_1252_and_keeps_every_byte() {
    // Shift-JIS "日本" - not valid UTF-8, and nothing we can name without a
    // statistical detector. It must still open (as mojibake) and save unchanged,
    // rather than being refused or rewritten as UTF-8 (SPEC §10.1).
    let bytes = b"\x93\xFA\x96\x7B\n";
    let Loaded { text, format, .. } = load(bytes);
    assert_eq!(format.encoding_name(), "windows-1252");
    assert!(!text.is_empty());
    assert_eq!(roundtrip(bytes), bytes);
}

#[test]
fn every_single_byte_survives_the_windows_1252_fallback() {
    // The fallback is only safe because windows-1252 maps all 256 bytes to
    // distinct characters under the WHATWG index. If that ever stopped holding,
    // every unnamed-encoding file would be silently corrupted on save.
    let bytes: Vec<u8> = (0u8..=255).filter(|b| *b != b'\r').collect();
    assert_eq!(roundtrip(&bytes), bytes);
}

#[test]
fn non_utf8_past_the_sample_window_still_keeps_its_bytes() {
    // The pathological case for sampled detection: 8 KiB of clean ASCII, then a
    // latin-1 byte. The prefix says UTF-8; decoding the whole file says otherwise,
    // and taking the decode's word for it is what keeps the byte intact.
    let mut bytes = vec![b'a'; SAMPLE + 16];
    bytes.push(0xE9); // 'é' in latin-1
    let Loaded { format, .. } = load(&bytes);
    assert_eq!(format.encoding_name(), "windows-1252");
    assert_eq!(roundtrip(&bytes), bytes);
}

#[test]
fn a_multibyte_character_split_by_the_sample_window_is_still_utf8() {
    // Land a 3-byte character across the sample boundary: `from_utf8` reports an
    // incomplete tail, which is the sampler's fault and says nothing about the file.
    let mut bytes = vec![b'a'; SAMPLE - 1];
    bytes.extend_from_slice("語".as_bytes());
    let Loaded { text, format, .. } = load(&bytes);
    assert_eq!(format.encoding_name(), "UTF-8");
    assert!(text.ends_with('語'));
}

#[test]
fn a_corrupt_bom_file_is_left_as_decoded_rather_than_reinterpreted() {
    // A BOM is a statement of fact: an unpaired surrogate makes this file corrupt,
    // not secretly windows-1252, so the fallback must not fire.
    let bytes = [0xFF, 0xFE, 0x00, 0xD8]; // BOM + a lone high surrogate
    let loaded = load(&bytes);
    assert_eq!(loaded.format.encoding_name(), "UTF-16LE");
    assert_eq!(loaded.text, "\u{FFFD}");
    // ...and it says so, so the caller can open it read-only rather than let a
    // save write that replacement character over the original bytes.
    assert!(loaded.lossy);
}

#[test]
fn a_file_that_decodes_cleanly_is_never_marked_lossy() {
    assert!(!load(b"plain\n").lossy);
    assert!(!load(b"\xEF\xBB\xBFwith a bom\n").lossy);
    // The windows-1252 fallback cannot fail, so a file that lands there is exact.
    assert!(!load(b"caf\xE9\n").lossy);
}

#[test]
fn final_newline_is_appended_only_when_missing() {
    let format = FileFormat::default();
    assert_eq!(format.encode("no newline", true).unwrap(), b"no newline\n");
    assert_eq!(format.encode("has one\n", true).unwrap(), b"has one\n");
    assert_eq!(format.encode("no newline", false).unwrap(), b"no newline");
}

#[test]
fn final_newline_uses_the_files_own_terminator() {
    let format = FileFormat {
        encoding: UTF_8,
        bom: false,
        eol: LineEnding::Crlf,
    };
    assert_eq!(format.encode("a", true).unwrap(), b"a\r\n");
}

#[test]
fn an_unrepresentable_character_refuses_the_write_and_names_itself() {
    let format = FileFormat {
        encoding: SHIFT_JIS,
        bom: false,
        eol: LineEnding::Lf,
    };
    let err = format
        .encode("hello 😀", false)
        .expect_err("an emoji has no Shift-JIS encoding");
    assert!(err.contains("Shift_JIS"), "message: {err}");
    assert!(err.contains('😀'), "message: {err}");
}

#[test]
fn an_unnameable_culprit_still_produces_an_honest_refusal() {
    // The probe walks characters, so a stateful encoding that rejects a *sequence*
    // whose characters each encode alone would leave it empty. Rare enough that no
    // encoding here reaches it through `encode`, but the message must still name
    // the encoding rather than claim a character it could not find.
    let format = FileFormat {
        encoding: SHIFT_JIS,
        bom: false,
        eol: LineEnding::Lf,
    };
    let message = format.unmappable_message("everything here encodes fine");
    assert!(message.contains("Shift_JIS"), "message: {message}");
}

#[test]
fn a_nul_byte_marks_a_file_binary() {
    assert!(is_binary(b"\x89PNG\x00\x1a"));
    assert!(!is_binary(b"plain text\n"));
    assert!(!is_binary(b"")); // an empty file is an empty text file
    // Latin-1 is not UTF-8, but it is text.
    assert!(!is_binary(b"caf\xE9"));
}

#[test]
fn a_bom_overrides_the_nul_test() {
    // UTF-16 text is half NUL bytes; its BOM is what says so.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend_from_slice(&u16::to_le_bytes(b'a' as u16));
    assert!(!is_binary(&bytes));
}

#[test]
fn a_nul_past_the_sample_window_does_not_count() {
    // Same bound as detection: the guard is a sampled heuristic, not a scan of a
    // 300 MB file (SPEC §10.4).
    let mut bytes = vec![b'a'; SAMPLE + 1];
    bytes.push(0);
    assert!(!is_binary(&bytes));
}

#[test]
fn line_ending_names_are_what_the_status_bar_shows() {
    assert_eq!(LineEnding::Lf.name(), "LF");
    assert_eq!(LineEnding::Crlf.name(), "CRLF");
}
