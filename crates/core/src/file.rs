//! A file's on-disk *form*: character encoding, line terminator, trailing
//! newline (SPEC §10.1).
//!
//! The rope always holds **UTF-8 with LF terminators** - every motion, column and
//! edit rule in the core is written against that one shape, and a second shape
//! would have to be handled by all of them. A file stored differently is
//! converted on the way in and converted back on the way out; [`FileFormat`] is
//! what remembers how. Opening a CRLF Shift-JIS file and saving it must not
//! silently rewrite it as an LF UTF-8 file.
//!
//! **Detection is sampled** (SPEC §10.4): the BOM plus a bounded prefix decide
//! both the encoding and the line ending, never a whole-file statistical pass.
//! Decoding itself is inherently whole-file for an in-RAM Tier-1/2 buffer, but it
//! is one pass, not two.
//!
//! Round-tripping is the property that matters, and it is what the tests assert:
//! `encode(load(bytes))` returns the original bytes for any file this module can
//! load, including one whose encoding it had to guess.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use serde::{Deserialize, Serialize};

/// Bytes of a file inspected to decide its encoding and line ending. Bounded so
/// opening a 300 MB log does not pay a statistical pass over the whole file
/// (SPEC §10.4); 8 KiB is far past the point where either signal stabilizes.
const SAMPLE: usize = 8 * 1024;

/// The line terminator a file uses on disk (SPEC §10.1). Internally every buffer
/// uses `Lf`; this is what a save converts back to.
///
/// Classic-Mac lone `\r` is deliberately not a variant: it is effectively extinct,
/// and a file holding stray `\r`s round-trips byte-exactly anyway because only
/// `\r\n` pairs are rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    /// The short name a status bar shows (`"LF"` / `"CRLF"`).
    pub fn name(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

/// How a buffer's bytes are laid out on disk, remembered from the load so the
/// save can reproduce it (SPEC §10.1).
///
/// The encoding is kept as an `encoding_rs` handle rather than an owned name, so
/// it is `Copy` and carrying one on every snapshot costs nothing; it stays private
/// so `encoding_rs` does not become part of `vortex-core`'s public vocabulary -
/// callers get [`Self::encoding_name`], which is all a status bar needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFormat {
    encoding: &'static Encoding,
    /// The file began with a byte-order mark. Kept separately from the encoding
    /// because a UTF-8 file with a BOM and one without are the same encoding but
    /// not the same bytes, and adding or dropping a BOM behind the user's back is
    /// exactly the silent rewrite §10.1 forbids.
    pub bom: bool,
    pub eol: LineEnding,
}

impl Default for FileFormat {
    /// UTF-8, no BOM, LF - what an unnamed scratch buffer writes if it is saved.
    fn default() -> Self {
        Self {
            encoding: UTF_8,
            bom: false,
            eol: LineEnding::Lf,
        }
    }
}

impl FileFormat {
    /// The encoding's canonical name (`"UTF-8"`, `"Shift_JIS"`), for the status bar.
    /// The only way out for the encoding, which stays private so `encoding_rs` does
    /// not become part of `vortex-core`'s public vocabulary.
    pub fn encoding_name(&self) -> &'static str {
        self.encoding.name()
    }

    /// Convert buffer text back to the file's own bytes.
    ///
    /// `final_newline` appends a trailing LF when the text lacks one (SPEC §10.1's
    /// POSIX-style default). An *empty* buffer is left empty - writing a lone
    /// newline into a file the user emptied on purpose is not "ensuring", it is
    /// editing. The buffer is never touched, only the bytes written, so this can
    /// never show up as a spurious unsaved change.
    ///
    /// Fails when the text holds a character the encoding cannot represent (an
    /// emoji typed into a Shift-JIS file). Refusing is the only non-lossy answer:
    /// `encoding_rs` would substitute an HTML numeric reference, writing
    /// `&#128512;` into the user's source file, and the WHATWG "replace with a
    /// question mark" alternative is no better (SPEC §8: never lose data silently).
    pub fn encode(&self, text: &str, final_newline: bool) -> Result<Vec<u8>, String> {
        let mut out = Cow::Borrowed(text);
        if final_newline && !out.is_empty() && !out.ends_with('\n') {
            out = Cow::Owned(format!("{out}\n"));
        }
        if self.eol == LineEnding::Crlf {
            // Collapse first: text pasted into a CRLF buffer may already hold
            // `\r\n`, and a bare `\n` -> `\r\n` pass would turn those into `\r\r\n`.
            out = Cow::Owned(out.replace("\r\n", "\n").replace('\n', "\r\n"));
        }

        let mut bytes = if is(self.encoding, UTF_16LE) || is(self.encoding, UTF_16BE) {
            // `encoding_rs` cannot *encode* UTF-16: `Encoding::encode` follows the
            // WHATWG "output encoding" rule and silently produces UTF-8 for these
            // two, which would rewrite the file in a different encoding than the
            // one we promised to preserve. Encoding UTF-16 by hand is exact and
            // total - every Rust `str` is valid Unicode scalar values.
            utf16_bytes(&out, is(self.encoding, UTF_16BE))
        } else {
            let (encoded, _, unmappable) = self.encoding.encode(&out);
            if unmappable {
                return Err(self.unmappable_message(&out));
            }
            encoded.into_owned()
        };

        if self.bom {
            let bom = bom_bytes(self.encoding);
            bytes.splice(0..0, bom.iter().copied());
        }
        Ok(bytes)
    }

    /// Name the first character the encoding cannot represent, so the error tells
    /// the user *what* to fix rather than only that something is wrong. Walks the
    /// text character by character, which is affordable because it runs only on the
    /// failing save, never on a successful one.
    fn unmappable_message(&self, text: &str) -> String {
        let name = self.encoding.name();
        let mut buf = [0u8; 4];
        let culprit = text
            .chars()
            .find(|c| self.encoding.encode(c.encode_utf8(&mut buf)).2);
        match culprit {
            Some(c) => format!("cannot write as {name}: {c:?} is not representable in it"),
            // A stateful encoding can reject a sequence whose characters each
            // encode alone. Rare, and still an honest refusal.
            None => format!("cannot write as {name}: the text is not representable in it"),
        }
    }
}

/// The encodings offered when choosing one by hand (SPEC §10.1).
///
/// A curated list, not everything `encoding_rs` implements: the Encoding Standard
/// defines dozens, most of them aliases or legacy single-byte variants nobody picks
/// from a menu, and a picker's job is to be *choosable*. These are the ones a file
/// in the wild is actually saved in, plus the two the detector can name on its own.
///
/// Names are WHATWG labels, so they round-trip through [`Encoding::for_label`] and
/// read the way the status bar prints them.
pub const OFFERED_ENCODINGS: &[&str] = &[
    "UTF-8",
    "UTF-16LE",
    "UTF-16BE",
    "windows-1252",
    "ISO-8859-2",
    "ISO-8859-5",
    "ISO-8859-7",
    "ISO-8859-15",
    "windows-1251",
    "windows-1256",
    "KOI8-R",
    "Shift_JIS",
    "EUC-JP",
    "EUC-KR",
    "GBK",
    "gb18030",
    "Big5",
];

impl FileFormat {
    /// Rewrite this format to use the encoding `label` names, keeping the line
    /// ending. Fails on a label the Encoding Standard does not know.
    ///
    /// The BOM follows the encoding rather than being carried across: it is only
    /// meaningful for the three encodings that can have one, and a UTF-8 file that
    /// had a BOM has no business keeping it once it is Shift-JIS. Switching *to*
    /// UTF-16 adds one, because a UTF-16 file without a BOM is a file nothing -
    /// including this editor - can detect.
    pub fn with_encoding(self, label: &str) -> Result<Self, String> {
        let encoding = Encoding::for_label(label.as_bytes())
            .ok_or_else(|| format!("unknown encoding: {label}"))?;
        let utf16 = is(encoding, UTF_16LE) || is(encoding, UTF_16BE);
        Ok(Self {
            encoding,
            bom: utf16 || (self.bom && is(encoding, UTF_8)),
            ..self
        })
    }
}

/// Whether `bytes` are binary rather than text, and so should not be opened in a
/// text editor at all (SPEC §10.3).
///
/// The test is a NUL byte in the sampled prefix, which is what `git` and `grep`
/// use and is the only one that is both cheap and hard to argue with: text does
/// not contain NUL. A BOM overrides it, because UTF-16 text is *full* of NULs and
/// its BOM says so explicitly.
///
/// This exists because of what [`load`]'s windows-1252 fallback would otherwise
/// do: it decodes *any* byte string successfully, so without a guard a PNG opens
/// as several megabytes of mojibake. It would even save back unharmed - until the
/// first edit, which silently corrupts the file.
pub fn is_binary(bytes: &[u8]) -> bool {
    Encoding::for_bom(bytes).is_none() && bytes[..bytes.len().min(SAMPLE)].contains(&0)
}

/// A decoded file: the editable text, the format that writes it back, and whether
/// anything was lost getting here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// UTF-8 with LF terminators - the one shape the buffer ever holds.
    pub text: String,
    /// What [`FileFormat::encode`] needs to reproduce the file.
    pub format: FileFormat,
    /// The decode hit bytes it could not interpret and substituted U+FFFD, so
    /// `text` is **not** a faithful copy of the file and encoding it would not
    /// reproduce those bytes. Only reachable for a file whose BOM declares an
    /// encoding it then violates (a truncated UTF-16 file): everything else either
    /// decodes cleanly or falls back to windows-1252, which cannot fail. The caller
    /// opens such a buffer read-only rather than letting a save overwrite the bytes
    /// that did not survive (SPEC §8, §10.3).
    pub lossy: bool,
}

/// Decode `bytes` into editable UTF-8/LF text plus the [`FileFormat`] that writes
/// it back unchanged (SPEC §10.1).
///
/// Encoding is decided by BOM first - a BOM is a statement of fact, not a guess -
/// then by whether a bounded prefix is valid UTF-8, and finally by falling back to
/// windows-1252. That last step is the deliberately dumb one: windows-1252 maps
/// all 256 bytes to distinct characters under the WHATWG index, so a file in an
/// encoding we cannot name still **round-trips byte-exactly** and is editable
/// (as mojibake) instead of refusing to open at all, which is what the core did
/// before. A statistical detector (`chardetng`) would name more of them correctly;
/// it is a dependency this has not needed to justify yet.
pub fn load(bytes: &[u8]) -> Loaded {
    let (encoding, from_bom) = match Encoding::for_bom(bytes) {
        Some((encoding, _)) => (encoding, true),
        None => (sniff(bytes), false),
    };

    let (text, malformed) = encoding.decode_with_bom_removal(bytes);
    // A sampled guess can only be wrong one way: a file whose first 8 KiB are
    // clean ASCII but whose tail is not UTF-8 (one latin-1 name late in a log).
    // Re-reading it as windows-1252 keeps every byte, where the replacement
    // characters a lossy decode leaves behind would be written back over the
    // user's data on the next save. A BOM'd file is left as decoded: a corrupt
    // UTF-16 file is corrupt, and reinterpreting it as bytes would be nonsense -
    // it opens read-only instead, so what did not survive is never written back.
    let (text, encoding, lossy) = if malformed && !from_bom {
        (
            WINDOWS_1252.decode_with_bom_removal(bytes).0,
            WINDOWS_1252,
            false,
        )
    } else {
        (text, encoding, malformed)
    };

    let eol = line_ending(&text);
    let text = match eol {
        LineEnding::Crlf => text.replace("\r\n", "\n"),
        LineEnding::Lf => text.into_owned(),
    };
    Loaded {
        text,
        format: FileFormat {
            encoding,
            bom: from_bom,
            eol,
        },
        lossy,
    }
}

/// The encoding of a file with no BOM: UTF-8 if a bounded prefix decodes as UTF-8,
/// else windows-1252 (see [`load`]).
fn sniff(bytes: &[u8]) -> &'static Encoding {
    let head = &bytes[..bytes.len().min(SAMPLE)];
    match std::str::from_utf8(head) {
        Ok(_) => UTF_8,
        // The sample cut a multi-byte character in half - the sample's fault, not
        // the file's, so it says nothing against UTF-8.
        Err(e) if e.error_len().is_none() => UTF_8,
        Err(_) => WINDOWS_1252,
    }
}

/// The dominant line terminator over a bounded prefix (SPEC §10.1, §10.4). Ties
/// and newline-free text are `Lf`: it is the internal form, so it is also the
/// answer that converts nothing.
fn line_ending(text: &str) -> LineEnding {
    let head = sample(text);
    let crlf = head.matches("\r\n").count();
    let lf = head.matches('\n').count() - crlf;
    if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// The first [`SAMPLE`] bytes of `text`, backed off to a character boundary so the
/// slice is valid.
fn sample(text: &str) -> &str {
    let mut end = text.len().min(SAMPLE);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Encode `s` as UTF-16 code units in the requested byte order (see
/// [`FileFormat::encode`] for why this is not `encoding_rs`'s job).
fn utf16_bytes(s: &str, big_endian: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        let pair = if big_endian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        };
        out.extend_from_slice(&pair);
    }
    out
}

/// The byte-order mark for an encoding that can carry one. Only the three
/// `Encoding::for_bom` recognizes reach here, so UTF-8's is the right default.
fn bom_bytes(encoding: &'static Encoding) -> &'static [u8] {
    if is(encoding, UTF_16LE) {
        &[0xFF, 0xFE]
    } else if is(encoding, UTF_16BE) {
        &[0xFE, 0xFF]
    } else {
        &[0xEF, 0xBB, 0xBF]
    }
}

/// Encoding identity. `encoding_rs` hands out one `&'static` per encoding, so
/// pointer equality *is* equality - and it is the comparison the crate documents.
fn is(a: &'static Encoding, b: &'static Encoding) -> bool {
    std::ptr::eq(a, b)
}

#[cfg(test)]
#[path = "file_tests.rs"]
mod tests;
