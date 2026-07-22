//! Theme files: the color-scheme format, where themes live, and how they load
//! (SPEC §10.5).
//!
//! A theme is a TOML file naming a [`Style`] for each chrome slot the frontend
//! paints. The file's **stem is its name** - `undertow.toml` is the theme
//! `undertow` - so listing the available themes never has to parse them.
//!
//! Themes come from two places, and both are always listed:
//! - the four **built-ins**, compiled into the binary with `include_str!`, so a
//!   fresh install has themes with an empty config directory and nothing is ever
//!   written to the user's disk unasked;
//! - **`$XDG_CONFIG_HOME/vortex/themes/*.toml`** (or `~/.config/vortex/themes`),
//!   where a user file *shadows* a built-in of the same name - the way you edit a
//!   shipped theme is to copy it there.
//!
//! Every slot is optional: an absent one inherits [`Theme::default`], so a user
//! theme can be three lines that recolor the selection and nothing else. A slot
//! that *is* present replaces its default wholesale - `current_line = { bg = ... }`
//! sets a background and no foreground, rather than merging one in.
//!
//! Parsing is deliberately **lazy**: [`discover`] only lists names, and [`load`]
//! reads and parses one theme when it is actually chosen. A broken user file
//! therefore costs an error toast at the moment you pick it, not a failure at
//! startup or a theme missing from the list with no explanation.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use crate::config::Theme;

/// The theme a fresh install starts in, and the name [`Config::default`] carries.
///
/// [`Config::default`]: crate::config::Config::default
pub const DEFAULT: &str = "undertow";

/// The themes shipped with the editor, compiled in so the picker is never empty.
/// Kept sorted by name; [`discover`] re-sorts anyway once user files join them.
const BUILTIN: &[(&str, &str)] = &[
    ("daylight", include_str!("../themes/daylight.toml")),
    ("instrument", include_str!("../themes/instrument.toml")),
    ("phosphor", include_str!("../themes/phosphor.toml")),
    ("undertow", include_str!("../themes/undertow.toml")),
];

/// Where a listed theme's TOML will be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Compiled into the binary; the payload is the file's text.
    Builtin(&'static str),
    /// A file under the user's themes directory.
    User(PathBuf),
}

/// One theme offered to the picker: its name (the file stem) and where it lives.
/// Carries no parsed styles - see the laziness note in the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub origin: Origin,
}

/// The user's themes directory: `$XDG_CONFIG_HOME/vortex/themes`, else
/// `$HOME/.config/vortex/themes`. `None` when neither variable is set (an
/// environment with no home to speak of), which simply means built-ins only.
///
/// XDG is used on every platform, macOS included: that is what terminal editors
/// do, and it keeps a dotfiles repo portable.
fn user_dir() -> Option<PathBuf> {
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").and_then(non_empty) {
        return Some(PathBuf::from(xdg).join("vortex").join("themes"));
    }
    let home = std::env::var_os("HOME").and_then(non_empty)?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("vortex")
            .join("themes"),
    )
}

/// Every theme available to the picker: the built-ins, plus any `*.toml` in the
/// user's themes directory, sorted by name.
pub fn discover() -> Vec<Entry> {
    discover_in(user_dir().as_deref())
}

/// [`discover`] against an explicit directory, so the shadowing rule is testable
/// without touching the real `$HOME`. A missing or unreadable directory is not an
/// error - it yields the built-ins alone (SPEC §8: degrade, never fail hard).
fn discover_in(dir: Option<&Path>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = BUILTIN
        .iter()
        .map(|&(name, text)| Entry {
            name: name.to_string(),
            origin: Origin::Builtin(text),
        })
        .collect();
    if let Some(dir) = dir {
        // `read_dir` errors (no such directory, no permission) flatten away to an
        // empty iterator, as do unreadable individual entries.
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match entries.iter_mut().find(|e| e.name == name) {
                // A user file wins over the built-in it shares a name with.
                Some(existing) => existing.origin = Origin::User(path),
                None => entries.push(Entry {
                    name: name.to_string(),
                    origin: Origin::User(path),
                }),
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Read and parse one discovered theme. The error is already user-facing (it names
/// the theme and the offending field), so a caller can put it straight in a toast.
pub fn load(entry: &Entry) -> Result<Theme, String> {
    let text = match &entry.origin {
        Origin::Builtin(text) => (*text).to_string(),
        Origin::User(path) => {
            fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    parse(&text).map_err(|e| format!("{}: {e}", entry.name))
}

/// Look a theme up by name and load it - what a `set_theme` command runs.
///
/// Re-runs [`discover`] rather than caching, so a theme file edited while the
/// editor is open takes effect the next time it is picked. The directory holds a
/// handful of small files, and this is on a keypress, not a frame.
pub fn load_named(name: &str) -> Result<Theme, String> {
    let entry = discover()
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("{name}: no such theme"))?;
    load(&entry)
}

/// Parse a theme file's text into the styles the frontend paints with.
///
/// Every error out of here is [`one_line`]d, because the only place these are shown
/// is a toast: a single row, of one screen width.
pub fn parse(text: &str) -> Result<Theme, String> {
    let file: ThemeFile = toml::from_str(text).map_err(|e| one_line(&e.to_string()))?;
    file.resolve().map_err(|e| one_line(&e))
}

/// Longest error kept. Both error sources quote the file back at the user - `toml`
/// echoes the offending line, and a bad color echoes the value - so a theme file
/// that is not really a theme (a binary, a minified blob) would otherwise carry a
/// megabyte-long line into a surface that can show one row of it.
const MAX_ERROR: usize = 200;

/// Flatten an error onto one line and cap its length.
///
/// A `toml` parse error is a caret diagram - `TOML parse error at line 2, column
/// 10`, then `  |`, `2 | <the offending source>`, `  | ^`, then the explanation. The
/// diagram rows are noise once the newlines are gone, and the source row would echo
/// the file's own contents onto the screen, so only the prose rows are kept.
fn one_line(error: &str) -> String {
    let mut out = error
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('|') && !line.contains(" | "))
        .collect::<Vec<_>>()
        .join(": ");
    if out.len() > MAX_ERROR {
        // Truncate on a character boundary, never mid-UTF-8.
        let end = (0..=MAX_ERROR)
            .rev()
            .find(|&n| out.is_char_boundary(n))
            .unwrap_or(0);
        out.truncate(end);
        out.push('…');
    }
    out
}

/// One style slot as written in a theme file. Colors are `"#rrggbb"` strings;
/// attributes are booleans that default to off.
///
/// Unknown keys are rejected rather than ignored, so a typo (`bg`/`bgcolor`,
/// `bold`/`weight`) is reported instead of silently doing nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StyleSpec {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    dim: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    underlined: bool,
    #[serde(default)]
    reversed: bool,
}

/// A theme file: one optional [`StyleSpec`] per slot of [`Theme`]. The field names
/// are the file's keys, so this struct *is* the format's documentation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    text: Option<StyleSpec>,
    head_bar: Option<StyleSpec>,
    status_bar: Option<StyleSpec>,
    gutter: Option<StyleSpec>,
    gutter_current: Option<StyleSpec>,
    selection: Option<StyleSpec>,
    current_line: Option<StyleSpec>,
    secondary_cursor: Option<StyleSpec>,
    toast_info: Option<StyleSpec>,
    toast_error: Option<StyleSpec>,
    palette: Option<StyleSpec>,
    palette_selected: Option<StyleSpec>,
}

impl ThemeFile {
    /// Turn the parsed file into a [`Theme`], falling back to the built-in default
    /// for every slot the file left out.
    fn resolve(self) -> Result<Theme, String> {
        let base = Theme::default();
        Ok(Theme {
            text: slot("text", self.text, base.text)?,
            head_bar: slot("head_bar", self.head_bar, base.head_bar)?,
            status_bar: slot("status_bar", self.status_bar, base.status_bar)?,
            gutter: slot("gutter", self.gutter, base.gutter)?,
            gutter_current: slot("gutter_current", self.gutter_current, base.gutter_current)?,
            selection: slot("selection", self.selection, base.selection)?,
            current_line: slot("current_line", self.current_line, base.current_line)?,
            secondary_cursor: slot(
                "secondary_cursor",
                self.secondary_cursor,
                base.secondary_cursor,
            )?,
            toast_info: slot("toast_info", self.toast_info, base.toast_info)?,
            toast_error: slot("toast_error", self.toast_error, base.toast_error)?,
            palette: slot("palette", self.palette, base.palette)?,
            palette_selected: slot(
                "palette_selected",
                self.palette_selected,
                base.palette_selected,
            )?,
        })
    }
}

/// Resolve one slot: an absent spec inherits `fallback`, a present one replaces it.
/// `field` only names the slot in error messages.
fn slot(field: &str, spec: Option<StyleSpec>, fallback: Style) -> Result<Style, String> {
    let Some(spec) = spec else {
        return Ok(fallback);
    };
    let mut style = Style::new();
    if let Some(fg) = &spec.fg {
        style = style.fg(color(field, "fg", fg)?);
    }
    if let Some(bg) = &spec.bg {
        style = style.bg(color(field, "bg", bg)?);
    }
    for (on, modifier) in [
        (spec.bold, Modifier::BOLD),
        (spec.dim, Modifier::DIM),
        (spec.italic, Modifier::ITALIC),
        (spec.underlined, Modifier::UNDERLINED),
        (spec.reversed, Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    Ok(style)
}

/// Parse a `"#rrggbb"` color into a true-color [`Color::Rgb`].
///
/// Hex only, on purpose: a *named* ANSI color is remapped by the user's terminal
/// profile, so a theme that used one could not promise the contrast it was designed
/// with (the same reasoning that already pins the built-in selection to RGB).
fn color(field: &str, key: &str, spec: &str) -> Result<Color, String> {
    let bad = || format!("{field}.{key}: expected a \"#rrggbb\" color, got {spec:?}");
    // `is_ascii` guarantees the byte ranges below land on character boundaries.
    let hex = spec
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.is_ascii());
    let hex = hex.ok_or_else(bad)?;
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16).map_err(|_| bad()) //
    };
    Ok(Color::Rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn every_builtin_theme_parses() {
        for &(name, text) in BUILTIN {
            let theme = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            // A theme that parsed but set nothing would silently be the default.
            assert_ne!(
                theme.text,
                Style::new(),
                "{name} leaves the editor ground unset"
            );
        }
    }

    #[test]
    fn the_default_theme_is_the_undertow_file() {
        // `Theme::default()` is hand-written Rust (so startup cannot fail) while
        // `undertow.toml` is what a user reads and copies. This holds the two to
        // being the same theme rather than to a comment saying they are.
        let undertow = BUILTIN
            .iter()
            .find(|&&(name, _)| name == DEFAULT)
            .expect("the default theme is built in");
        assert_eq!(parse(undertow.1).unwrap(), Theme::default());
    }

    #[test]
    fn an_absent_slot_inherits_the_default() {
        let theme = parse(r##"selection = { bg = "#010203" }"##).unwrap();
        assert_eq!(theme.selection.bg, Some(Color::Rgb(1, 2, 3)));
        // Untouched slots are the default theme's, and a *present* slot replaces
        // rather than merges: this one names no fg, so it has none.
        assert_eq!(theme.selection.fg, None);
        assert_eq!(theme.head_bar, Theme::default().head_bar);
        // An empty file is a legal theme: it is the default, verbatim.
        assert_eq!(parse("").unwrap(), Theme::default());
    }

    #[test]
    fn attributes_map_onto_modifiers() {
        let theme = parse(
            r##"
            head_bar = { fg = "#ffffff", bold = true, italic = true }
            gutter = { dim = true, underlined = true, reversed = true }
            "##,
        )
        .unwrap();
        assert_eq!(theme.head_bar.fg, Some(Color::Rgb(255, 255, 255)));
        assert!(theme.head_bar.add_modifier.contains(Modifier::BOLD));
        assert!(theme.head_bar.add_modifier.contains(Modifier::ITALIC));
        assert!(theme.gutter.add_modifier.contains(Modifier::DIM));
        assert!(theme.gutter.add_modifier.contains(Modifier::UNDERLINED));
        assert!(theme.gutter.add_modifier.contains(Modifier::REVERSED));
        // An attribute left out stays off rather than inheriting.
        assert!(!theme.gutter.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_bad_color_names_the_field_that_carries_it() {
        // Every rejection path: not a hex string, wrong length, non-hex digits, and
        // a non-ASCII string of the right byte length (which must not panic on the
        // byte slicing).
        for bad in [
            r##"selection = { bg = "blue" }"##,
            r##"selection = { bg = "#abc" }"##,
            r##"selection = { fg = "#gggggg" }"##,
            r##"selection = { bg = "#éé" }"##,
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("selection."), "unhelpful error: {err}");
            assert!(err.contains("#rrggbb"), "unhelpful error: {err}");
        }
    }

    #[test]
    fn an_unknown_key_is_rejected_not_ignored() {
        // A typo must be reported: silently ignoring it means the user stares at a
        // theme that "did not apply" with nothing to go on.
        let err = parse(r##"selection = { bgcolor = "#010203" }"##).unwrap_err();
        assert!(err.contains("bgcolor"), "{err}");
        let err = parse(r##"selektion = { bg = "#010203" }"##).unwrap_err();
        assert!(err.contains("selektion"), "{err}");
        // Malformed TOML surfaces as an error too, rather than a panic.
        assert!(parse("selection = {").is_err());
    }

    #[test]
    fn a_parse_error_is_one_line_and_does_not_quote_the_file() {
        // A toast is a single line, and `toml`'s error is a caret diagram. Anything
        // with a newline in it renders as garbage there.
        let err = parse("selection = {\nnonsense here\n").unwrap_err();
        assert!(!err.contains('\n'), "multi-line error in a toast: {err:?}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("expected"), "{err}");
        // The diagram quotes the offending source line; a theme file is not
        // necessarily one the user wrote (a stray symlink), so it stays off screen.
        assert!(!err.contains("nonsense here"), "{err}");
    }

    #[test]
    fn a_huge_error_is_truncated_on_a_character_boundary() {
        // A bad color quotes the value back, so a file with a megabyte-long "color"
        // would otherwise hand a megabyte-long string to the toast surface. The
        // repeated char is multi-byte, so a naive byte truncation would panic here.
        let err = parse(&format!(r#"selection = {{ bg = "{}" }}"#, "é".repeat(4096))).unwrap_err();
        assert!(err.len() <= MAX_ERROR + '…'.len_utf8(), "not truncated");
        assert!(err.ends_with('…'), "{err}");
        assert!(err.starts_with("selection.bg:"), "still names the field");
    }

    #[test]
    fn discovery_lists_the_builtins_when_there_is_no_user_directory() {
        let found = discover_in(None);
        assert_eq!(found.len(), BUILTIN.len());
        assert!(found.iter().any(|e| e.name == DEFAULT));
        assert!(found.iter().all(|e| matches!(e.origin, Origin::Builtin(_))));
        // Sorted by name, so the picker's order is stable across runs.
        let mut sorted = found.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found, sorted);
        // A directory that does not exist behaves like none at all.
        assert_eq!(discover_in(Some(Path::new("/nonexistent/vortex"))), found);
    }

    #[test]
    fn a_user_file_shadows_the_builtin_it_shares_a_name_with() {
        let dir = TempDir::new();
        dir.file("undertow.toml", r##"selection = { bg = "#010203" }"##);
        dir.file("midnight.toml", r##"selection = { bg = "#040506" }"##);
        // Not a theme: only `*.toml` is listed.
        dir.file("README.md", "not a theme");
        let found = discover_in(Some(&dir.path));

        assert_eq!(found.len(), BUILTIN.len() + 1, "one new theme, one shadow");
        let undertow = found.iter().find(|e| e.name == "undertow").unwrap();
        assert!(matches!(undertow.origin, Origin::User(_)));
        assert_eq!(
            load(undertow).unwrap().selection.bg,
            Some(Color::Rgb(1, 2, 3)),
            "the user's file is what loads"
        );
        let midnight = found.iter().find(|e| e.name == "midnight").unwrap();
        assert_eq!(
            load(midnight).unwrap().selection.bg,
            Some(Color::Rgb(4, 5, 6))
        );
        // The other built-ins are untouched by the shadowing.
        assert!(
            found
                .iter()
                .filter(|e| matches!(e.origin, Origin::Builtin(_)))
                .count()
                == BUILTIN.len() - 1
        );
    }

    #[test]
    fn loading_reports_a_broken_or_missing_theme_by_name() {
        let dir = TempDir::new();
        dir.file("broken.toml", r##"selection = { bg = "nonsense" }"##);
        let found = discover_in(Some(&dir.path));
        let broken = found.iter().find(|e| e.name == "broken").unwrap();
        let err = load(broken).unwrap_err();
        assert!(err.starts_with("broken:"), "{err}");

        // A file that vanished between listing and loading is an error, not a panic.
        let gone = Entry {
            name: "gone".into(),
            origin: Origin::User(dir.path.join("gone.toml")),
        };
        assert!(load(&gone).is_err());
        // As is a name that was never listed at all.
        assert!(load_named("no-such-theme").unwrap_err().contains("no such"));
    }

    #[test]
    fn load_named_finds_a_builtin() {
        // The path a `set_theme` command takes, end to end.
        assert_eq!(load_named(DEFAULT).unwrap(), Theme::default());
        assert_ne!(load_named("phosphor").unwrap(), Theme::default());
    }

    #[test]
    fn the_user_directory_follows_xdg_then_home() {
        // Not asserting on the *live* environment (that would depend on the machine
        // running the tests), just that the two conventions resolve as documented.
        let dir = user_dir();
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            assert_eq!(dir, Some(PathBuf::from(xdg).join("vortex").join("themes")));
        } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            assert_eq!(dir, Some(PathBuf::from(home).join(".config/vortex/themes")));
        } else {
            assert_eq!(dir, None);
        }
    }
}
