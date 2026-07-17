//! OSC 52: set the terminal's clipboard over the escape-sequence channel (SPEC §11).
//!
//! The core owns the clipboard *register*; the frontend bridges it to the OS
//! clipboard. OSC 52 is the bridge that works both locally and over SSH - the
//! terminal itself sets its host's clipboard - so it needs no native-clipboard
//! dependency (`arboard` et al.) and directly serves the remote-frontend future
//! (SPEC §0, §11). Write-only here: reading the clipboard back over OSC 52 is
//! widely unsupported/blocked, and paste-in arrives via bracketed paste instead.
//!
//! The sequence is `ESC ] 52 ; c ; <base64> BEL`: selection `c` is the clipboard
//! (vs `p`, primary), and the payload is base64 so arbitrary text - including the
//! control bytes that would otherwise terminate the sequence early - travels
//! escape-safely. We write it to the controlling terminal (`/dev/tty`) rather than
//! stdout so a redirected stdout never swallows the clipboard update; ratatui owns
//! stdout for the frame, so writing the sequence there could also interleave with a
//! paint.

use std::io::{self, Write};

/// Standard base64 alphabet (RFC 4648). OSC 52 payloads use standard, not URL-safe.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `input` as standard base64 with `=` padding. Encode-only: the OSC 52 write
/// path never decodes, so no decoder is carried. Kept dependency-free (a full base64
/// crate would be a new dep for ~15 lines, SPEC §3) and pure so it is unit-testable
/// against known vectors.
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        // Pack up to three bytes big-endian into a 24-bit group, then read it out as
        // four 6-bit indices. Missing bytes in the final chunk read as 0 and are
        // emitted as `=` padding, so the output length is always a multiple of four.
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let group = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[(group >> 18 & 0x3f) as usize] as char);
        out.push(BASE64[(group >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(group >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[(group & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The OSC 52 escape sequence that sets the clipboard to `text`. Split from the I/O
/// so the exact bytes on the wire are unit-testable without a terminal (SPEC §13).
fn sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Write the OSC 52 clipboard sequence for `text` to `out`. Split from [`copy`]'s
/// terminal-opening so the exact bytes written are unit-testable against any
/// [`Write`] (a `Vec<u8>`) without a controlling tty (SPEC §13).
fn write_to<W: Write>(out: &mut W, text: &str) -> io::Result<()> {
    out.write_all(sequence(text).as_bytes())?;
    out.flush()
}

/// Set the terminal clipboard to `text` via OSC 52 (SPEC §11). Writes to `/dev/tty`
/// so a redirected stdout cannot swallow it and it does not interleave with ratatui's
/// stdout frame. Best-effort by contract: the caller ignores the result (a terminal
/// that does not honor OSC 52 simply leaves the clipboard unchanged). Returns the I/O
/// error only so tests and a future diagnostics path can observe a failed open/write.
pub fn copy(text: &str) -> io::Result<()> {
    // The controlling terminal, so the sequence reaches the real terminal even when
    // stdout is a pipe. Not available in every environment (e.g. no controlling tty);
    // that surfaces as an error the caller treats as "clipboard unavailable".
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    write_to(&mut tty, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 §10 test vectors - the canonical base64 conformance set.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_multibyte_utf8() {
        // A non-ASCII payload must be encoded from its UTF-8 bytes, not chars, so it
        // round-trips through the terminal intact (SPEC §4 byte-canonical text).
        assert_eq!(base64_encode("é".as_bytes()), "w6k=");
        assert_eq!(base64_encode("語".as_bytes()), "6Kqe");
    }

    #[test]
    fn sequence_wraps_payload_in_osc52_framing() {
        // ESC ] 52 ; c ; <base64> BEL - selection `c` (clipboard), BEL terminator.
        assert_eq!(sequence("foo"), "\x1b]52;c;Zm9v\x07");
        assert_eq!(sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn sequence_base64s_control_bytes_rather_than_emitting_them_raw() {
        // The payload may contain a newline or even an ESC; base64 keeps those out of
        // the raw byte stream so they cannot terminate the sequence early.
        let seq = sequence("a\nb");
        assert!(!seq[5..].contains('\n'), "newline must not appear raw");
        assert_eq!(seq, "\x1b]52;c;YQpi\x07");
    }

    #[test]
    fn write_to_emits_the_full_sequence_and_flushes() {
        // The exact bytes the copy path puts on the wire, captured against an
        // in-memory writer - the part `copy` cannot test without a controlling tty.
        let mut out: Vec<u8> = Vec::new();
        write_to(&mut out, "hello").unwrap();
        assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07");
    }
}
