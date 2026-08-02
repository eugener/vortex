//! Frontend configuration - the seam where user settings enter the TUI.
//!
//! One resolved [`Config`] is built at startup from the user's `config.toml` and
//! threaded into the render and input paths (SPEC §10.5). Everything downstream
//! already read from this value while it was still the built-in `Default`, which is
//! why adding the file touched only this module.
//!
//! **Every field is optional and falls back to the built-in default**, so a config
//! file can be one line. **An unknown key is an error**, not a silent no-op - the
//! same rule theme files follow, and for the same reason: a typo must be reported,
//! or the user stares at a setting that "did not apply" with nothing to go on.
//! Because the config resolves before the terminal is even in raw mode, a broken
//! file cannot be reported as a toast; it degrades to the defaults and the message
//! is shown once the screen exists.
//!
//! Scope is *nearly* frontend-only: styling, tab width, and the keymap (key→intent
//! is frontend-owned per SPEC §2.2/§12.2) never cross the seam. The one exception is
//! the handful of settings the core is what acts on, which travel to it as
//! [`CoreOptions`](vortex_core::action::CoreOptions) - see [`Config::core_options`].

use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use vortex_core::action::CoreOptions;

use crate::keymap::Keymap;

/// The fewest characters a single complaint keeps when several share the one row a
/// toast has. Below this a message is all ellipsis and names nothing, so a row that
/// runs slightly long is the better failure - the point is to say which settings were
/// wrong, and a truncated list says less than a long one.
const MIN_COMPLAINT: usize = 40;

/// Default display width of a tab stop (SPEC §4), when the config file says
/// nothing. Four is the width the editor used before it was configurable.
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// How the gutter numbers its rows (SPEC §7.5 chrome, M8).
///
/// There is deliberately no third "pure relative" mode that prints `0` on the
/// cursor's own row. The number a relative gutter is *for* is the count you type
/// before a motion, and the one row you never need that count for is the one you
/// are on - so the slot is free to carry the absolute number, which is the thing a
/// relative gutter otherwise costs you (a jump target, a line to quote in a review).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineNumbers {
    /// Every row shows its own 1-based number.
    #[default]
    Absolute,
    /// Every row shows its distance from the cursor's row; the cursor's own row
    /// shows its absolute number.
    Relative,
}

/// All user-configurable settings, resolved once at startup and threaded into the
/// render and input paths. Grows as configurable surfaces land, so it is passed as
/// a whole rather than field-by-field (SPEC §10.5).
#[derive(Debug, Clone)]
pub struct Config {
    /// Colors/attributes for the non-text chrome.
    pub theme: Theme,
    /// Which theme [`Self::theme`] came from, so the picker can highlight the one
    /// in use and restore it when a preview is cancelled.
    pub theme_name: String,
    /// Key -> intent bindings: the built-in map with the config file's `[keys]`
    /// table applied over it.
    pub keymap: Keymap,
    /// Display width of a tab stop (SPEC §4). Frontend-owned because a tab's width
    /// is a rendering question - the buffer holds one byte either way.
    pub tab_width: usize,
    /// How the gutter numbers its rows (SPEC §7.5). Frontend-owned for the same
    /// reason the gutter itself is: the core has no idea a margin exists.
    pub line_numbers: LineNumbers,
    /// Display columns to draw a ruler down (SPEC §7.5), 0-based so a value of 80
    /// marks the 81st column - the first one *past* an 80-column limit, which is
    /// where a limit is actually crossed. Empty (the default) draws none.
    ///
    /// A list rather than one column because the limits a file is held to often
    /// come in pairs (a soft width and a hard one), and because the cost of drawing
    /// several is the same as drawing one.
    pub rulers: Vec<usize>,
    /// Draw a vertical rule at each indent level (SPEC §7.5). Off by default, like
    /// every piece of chrome that puts a mark on every row: the editor spends the
    /// screen on text unless asked otherwise.
    pub indent_guides: bool,
    /// Reserve the body's rightmost column for a scrollbar (SPEC §7.5). **On by
    /// default**, unlike the rest of the chrome here, which stays off because it marks
    /// every row. This one marks no row: it answers "where am I in something bigger
    /// than the screen", which is a question every editor's reader has and which
    /// nothing else on screen answers - the status bar's line number tells you where
    /// the *caret* is, not where the window is. One column is what that costs, and
    /// `scrollbar = false` still buys it back.
    ///
    /// The column is reserved whether or not a bar is drawn in it. A scrollbar that
    /// appeared only once a file outgrew the screen would slide every line one cell
    /// sideways at the moment the file crossed that boundary.
    pub scrollbar: bool,
    /// Pin the enclosing scopes of the viewport's top row at the top of the body
    /// (SPEC §7.5). Off by default, and the only piece of chrome that costs *rows*
    /// of text - as many as the code at the top of the screen is nested deep,
    /// capped by [`STICKY_CONTEXT_MAX`] and by a third of the body.
    pub sticky_context: bool,
    /// Append a trailing newline on save when the buffer lacks one (SPEC §10.1).
    /// Held here because this is where the user's file is read, and handed to the
    /// core, which is what acts on it.
    pub final_newline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            theme_name: crate::theme::DEFAULT.to_string(),
            keymap: Keymap::default(),
            tab_width: DEFAULT_TAB_WIDTH,
            line_numbers: LineNumbers::default(),
            rulers: Vec::new(),
            indent_guides: false,
            scrollbar: true,
            sticky_context: false,
            final_newline: CoreOptions::default().final_newline,
        }
    }
}

impl Config {
    /// The settings the *core* acts on, to send it as `Action::Configure`
    /// (SPEC §10.5). Split out here rather than assembled at the call site so the
    /// answer to "which settings cross the seam" lives with the settings.
    pub fn core_options(&self) -> CoreOptions {
        // Built from the default and adjusted, rather than named field by field:
        // `CoreOptions` is non-exhaustive, so a setting added there is a compile
        // error here only when it is one this side is meant to be sending.
        let mut options = CoreOptions::default();
        options.final_newline = self.final_newline;
        options
    }
}

/// The config file as written: every key optional, unknown keys rejected.
///
/// A separate type from [`Config`] because the two have genuinely different shapes.
/// The file names a theme; the resolved config carries the loaded [`Theme`] as well,
/// and a name that failed to load must still leave a working editor.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    theme: Option<String>,
    tab_width: Option<usize>,
    line_numbers: Option<LineNumbers>,
    rulers: Option<Vec<usize>>,
    indent_guides: Option<bool>,
    scrollbar: Option<bool>,
    sticky_context: Option<bool>,
    final_newline: Option<bool>,
    /// The keys table, merged over the built-in bindings rather than replacing them:
    /// a user who binds one key keeps the other fifty. Binding a chord that already
    /// exists overrides it, which is the whole point.
    ///
    /// Left as a raw `toml::Table` because its values have two shapes - a command
    /// name, or a *context* subtable (SPEC §10.5) - and a `serde` enum over the two
    /// would fail the whole file on one bad row. The keymap reads it row by row and
    /// reports each one it could not apply, so one typo costs one binding.
    #[serde(default)]
    keys: toml::Table,
}

/// The user's config file: `$XDG_CONFIG_HOME/vortex/config.toml`, else
/// `$HOME/.config/vortex/config.toml`. `None` in an environment with no home to
/// speak of, which simply means built-in defaults.
///
/// XDG on every platform, macOS included - the same rule the themes directory
/// follows, and what keeps a dotfiles repo portable.
pub fn user_path() -> Option<PathBuf> {
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    config_path(
        std::env::var_os("XDG_CONFIG_HOME").and_then(non_empty),
        std::env::var_os("HOME").and_then(non_empty),
    )
}

/// [`user_path`]'s rule, with the environment passed in - the environment is
/// process-global, and a test that sets it races every other test in the binary.
fn config_path(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg {
        return Some(PathBuf::from(xdg).join("vortex").join("config.toml"));
    }
    Some(
        PathBuf::from(home?)
            .join(".config")
            .join("vortex")
            .join("config.toml"),
    )
}

/// Resolve the config for this session: the file at `path` if it is there, else the
/// built-in defaults. Returns the config plus a message to show once there is a
/// screen to show it on.
///
/// **A broken config never stops the editor starting** (SPEC §8). A missing file is
/// not an error at all - it is the normal state of a fresh install. A file that
/// exists but does not parse, or that names a theme that will not load, degrades to
/// the defaults *for the parts that failed* and reports why.
/// `asked_for` distinguishes the two callers, and only matters when the file is not
/// there. A missing config in the default location is the normal state of a fresh
/// install and says nothing; a missing one named by `--config` is a typo, and starting
/// silently on the defaults leaves the user looking at an editor with none of their
/// settings and nothing anywhere saying why.
pub fn load(path: &Path, asked_for: bool) -> (Config, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let problem = asked_for.then(|| {
                format!(
                    "{}: no such config file",
                    crate::theme::one_line(&path.display().to_string())
                )
            });
            return (Config::default(), problem);
        }
        Err(err) => {
            return (
                Config::default(),
                Some(format!("{}: {err}", path.display())),
            );
        }
    };
    parse(&text)
}

/// [`load`]'s pure half: config text in, resolved config plus any complaint out.
///
/// Every message out of here is [`one_line`](crate::theme::one_line)d, because the
/// only place they are shown is a toast - one row, of one screen width. It also
/// bounds them: a `toml` error quotes the offending line back, and a bad binding
/// echoes the key the user wrote, so a config file that is not really a config file
/// (a binary, a minified blob) would otherwise carry a megabyte-long line into it.
fn parse(text: &str) -> (Config, Option<String>) {
    let file: ConfigFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(err) => {
            return (
                Config::default(),
                Some(crate::theme::one_line(&err.to_string())),
            );
        }
    };

    let mut config = Config::default();
    // Every complaint, not the last one. The settings here fail independently - a bad
    // tab width says nothing about the theme, which says nothing about the bindings -
    // so overwriting meant a file with three typos took three restarts to fix, each
    // one revealing the next. Joined into the single line the toast has room for.
    let mut problems: Vec<String> = Vec::new();
    if let Some(width) = file.tab_width {
        // Zero would divide the display-column math by nothing and collapse every
        // tab onto the previous glyph; there is no sane reading of it.
        if width == 0 {
            problems.push("tab_width must be at least 1".to_string());
        } else {
            config.tab_width = width;
        }
    }
    if let Some(mode) = file.line_numbers {
        config.line_numbers = mode;
    }
    if let Some(rulers) = file.rulers {
        config.rulers = rulers;
    }
    if let Some(on) = file.indent_guides {
        config.indent_guides = on;
    }
    if let Some(on) = file.scrollbar {
        config.scrollbar = on;
    }
    if let Some(on) = file.sticky_context {
        config.sticky_context = on;
    }
    if let Some(final_newline) = file.final_newline {
        config.final_newline = final_newline;
    }
    if let Some(name) = file.theme {
        // A theme that will not load leaves the built-in one in place: a bad color
        // in a config file must not mean an editor with no colors at all.
        match crate::theme::load_named(&name) {
            Ok(theme) => {
                config.theme = theme;
                config.theme_name = name;
            }
            Err(err) => problems.push(err),
        }
    }
    if !file.keys.is_empty() {
        // Every binding that parses is applied, whatever else in the table did not,
        // so one typo costs exactly the one key it is on.
        problems.extend(
            config
                .keymap
                .extend_from_table(&file.keys)
                .iter()
                .map(|err| err.to_string()),
        );
    }
    // Each part gets a share of the row rather than the join being cut to fit it. The
    // obvious version - bound each to a full row, join, bound the join - reads the same
    // and silently drops whichever complaints came last, which is the "three typos,
    // three restarts" problem again in a narrower window: one long theme error would
    // eat the binding error behind it. A floor keeps a share readable when there are
    // many, at the cost of a row that can run slightly long.
    let problem = (!problems.is_empty()).then(|| {
        let share = (crate::theme::MAX_ERROR / problems.len()).max(MIN_COMPLAINT);
        problems
            .iter()
            .map(|p| crate::theme::one_line_within(p, share))
            .collect::<Vec<_>>()
            .join("; ")
    });
    (config, problem)
}

/// Chrome styling for the frontend's non-text UI: the head/status bars and the
/// line-number gutter. Bundled into one value (not scattered `const`s) so a config
/// can swap it wholesale. `Copy` - each [`Style`] is `Copy` - so threading it per
/// frame is free and it never touches the render hot path beyond a field read.
///
/// Every field here is a key in a theme file ([`crate::theme`]); adding one means
/// adding it there too, and the round-trip test in that module holds the built-in
/// default and `themes/undertow.toml` to being the same theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The editor body's own ground: the background the text area is filled with
    /// and the foreground unstyled text takes. Painted as the base style beneath
    /// every row, so a theme is not at the mercy of the user's terminal background
    /// (a light theme in a black terminal would otherwise be unreadable).
    pub text: Style,
    /// Top bar: the bufferline's tab strip (left) and line count (right). This is
    /// the style of the *active* tab and of the bar itself.
    pub head_bar: Style,
    /// The active buffer's tab. Two jobs, and a theme is expected to use both: the
    /// tab is a **surface that has come forward**, so it takes a lifted ground, and
    /// it is **where you are**, so it takes the theme's state accent on its text.
    ///
    /// Carrying part of it in the background matters because foreground brightness
    /// is the first distinction to wash out on a poor terminal. Carrying only *part*
    /// of it there matters too: flooding a permanently-visible strip with a reserved
    /// accent spends on chrome what a theme saves for signals, and leaves the tab
    /// out-shouting the selection and the error toast.
    pub head_bar_active: Style,
    /// Tabs in the bufferline other than the active one - dimmed so they recede
    /// behind the filled one. Its own slot rather than a modifier applied to
    /// [`Self::head_bar`], because which pair of colors separates "current" from
    /// "background" is a theme's call (§10.5).
    pub head_bar_inactive: Style,
    /// The divider between adjacent tabs, and the `‹`/`›` overflow markers - the
    /// bufferline's chrome. Dim: it is there to stop two names reading as one, not
    /// to be looked at.
    pub head_bar_separator: Style,
    /// Bottom bar: cursor position (left) and buffer metrics (right).
    pub status_bar: Style,
    /// Gutter line numbers away from the cursor - dimmed so they recede.
    pub gutter: Style,
    /// The cursor line's gutter number - brightened/bold so the active row stands out.
    pub gutter_current: Style,
    /// Selected text. Uses explicit RGB (not named ANSI colors, which the terminal
    /// remaps to its own palette and can render as low-contrast light-on-light):
    /// a muted dark blue behind true white keeps a legible contrast on any theme.
    /// Once syntax coloring lands (M4) this may soften to let those foregrounds
    /// show through.
    pub selection: Style,
    /// The cursor line's background - a subtle tint filling the whole row so the
    /// active line is easy to find without pulling the eye like a selection does.
    pub current_line: Style,
    /// The ground of a ruler column (SPEC §7.5) - the vertical stripe at each
    /// configured line-length guide. Quieter than [`Self::current_line`]: a ruler is
    /// on screen for the whole session and every row at once, so it has to read as a
    /// margin rather than as a highlight. Distinct from it too, since the two cross
    /// on the caret's row and a ruler that matched would vanish exactly there.
    pub ruler: Style,
    /// The indent guide glyph (SPEC §7.5) - a foreground only, no ground of its own,
    /// so a selection or a current-line tint flows over it rather than being broken
    /// by it. Dimmer than the text it aligns: a guide is read out of the corner of the
    /// eye while scanning structure, and one loud enough to read directly would
    /// compete with the code for attention on every single row.
    pub indent_guide: Style,
    /// The scrollbar's track (SPEC §7.5) - the part of the column the thumb is *not*
    /// on. Quiet, since it is the background of a control rather than the control.
    pub scrollbar_track: Style,
    /// The scrollbar's thumb: the stretch of track standing for what is on screen.
    /// This is the part that carries the answer, so it is the part with the contrast.
    pub scrollbar_thumb: Style,
    /// The sticky context header's ground (SPEC §7.5) - the rows pinned above the
    /// text showing which scopes enclose it. A ground, unlike the indent guide's
    /// bare foreground, because these rows are *not* buffer text at the position
    /// they occupy: something has to say where the pinned lines stop and the file
    /// resumes, and a tint says it without spending a row on a separator. The
    /// syntax colors still paint over it, so a pinned line reads as the code it is.
    pub sticky_context: Style,
    /// The marker for a *secondary* (non-primary) caret in a multi-cursor set
    /// (SPEC §2.2). The terminal has a single real cursor, which the primary caret
    /// uses; the others are painted as a one-cell reversed block so they are visible.
    pub secondary_cursor: Style,
    /// Every other match of the live search (SPEC §11) - the ones you are not on.
    /// A wash, not an accent: on a `.` pattern this fills the screen, so it has to
    /// read as "these too" behind the text rather than compete with it.
    pub search_match: Style,
    /// The match the search is *on* - the one Enter would take you to, and the one a
    /// replace would rewrite. Distinct from [`Self::search_match`] because a screen
    /// of identically-marked hits does not answer "which one is next", which is the
    /// only question a live search is really asking.
    pub search_current: Style,
    /// Informational toasts (SPEC §7.5): file opened/saved. Calm, so they inform
    /// without alarming.
    pub toast_info: Style,
    /// Error toasts: save failed, edit rejected. High-contrast red so a failure is
    /// unmistakable (SPEC §8: a failure must be visible, never silent).
    pub toast_error: Style,
    /// The command palette box (SPEC §7.5): its border, query row, and unselected
    /// entries.
    pub palette: Style,
    /// The palette's highlighted row - an accent fill so the selection is obvious.
    pub palette_selected: Style,
    /// The four LSP diagnostic severities (SPEC §5). The `fg` colors the underline
    /// under a flagged span and the mark in the gutter; a background, if set, is
    /// ignored for the underline (which paints only the foreground) so a theme need
    /// not reserve one. Kept as four fields rather than a lookup so a theme file
    /// names each severity explicitly, the same as every other slot.
    pub diagnostic_error: Style,
    pub diagnostic_warning: Style,
    pub diagnostic_information: Style,
    pub diagnostic_hint: Style,
    /// Syntax highlighting colors (SPEC §5, M4). The core's ~18
    /// [`HighlightKind`](vortex_core::HighlightKind)s are painted from these eight
    /// roles (see [`Theme::highlight`]) rather than one field per kind: a coherent
    /// scheme is a handful of hues, not eighteen, and a theme file names eight keys
    /// instead of eighteen. Only the `fg` is used - a highlight colors the glyph and
    /// lets selection and current-line backgrounds show through.
    pub syntax_keyword: Style,
    pub syntax_function: Style,
    pub syntax_type: Style,
    pub syntax_string: Style,
    pub syntax_comment: Style,
    pub syntax_constant: Style,
    pub syntax_variable: Style,
    pub syntax_punctuation: Style,
}

impl Theme {
    /// The style for a diagnostic [`Severity`](vortex_core::Severity) - the seam's
    /// semantic tag resolved to concrete colors here, in the frontend, exactly as
    /// SPEC §5 requires (the core never names a color).
    pub fn diagnostic(&self, severity: vortex_core::Severity) -> Style {
        use vortex_core::Severity;
        match severity {
            Severity::Error => self.diagnostic_error,
            Severity::Warning => self.diagnostic_warning,
            Severity::Information => self.diagnostic_information,
            Severity::Hint => self.diagnostic_hint,
        }
    }

    /// The style for a syntax [`HighlightKind`](vortex_core::HighlightKind), the
    /// M4 twin of [`Theme::diagnostic`]: the core's semantic tag resolved to a color
    /// here. Related kinds collapse to one role (a macro and a call are both
    /// `syntax_function`, a builtin type and a user type both `syntax_type`) so the
    /// palette stays small. `HighlightKind` is `non_exhaustive`, so an unknown future
    /// kind falls back to the body text color - visible, unstyled, never a panic.
    pub fn highlight(&self, kind: vortex_core::HighlightKind) -> Style {
        use vortex_core::HighlightKind as K;
        match kind {
            K::Keyword => self.syntax_keyword,
            K::Function | K::Macro | K::Constructor => self.syntax_function,
            K::Type | K::TypeBuiltin => self.syntax_type,
            K::String | K::Escape => self.syntax_string,
            K::Comment => self.syntax_comment,
            K::Constant | K::ConstantBuiltin => self.syntax_constant,
            K::Variable | K::Parameter | K::Property => self.syntax_variable,
            K::Attribute | K::Label | K::Operator | K::Punctuation => self.syntax_punctuation,
            _ => self.text,
        }
    }
}

impl Default for Theme {
    /// The built-in theme: **undertow**, the house dark scheme (see
    /// `themes/undertow.toml`, whose every value this mirrors).
    ///
    /// Written out in Rust rather than parsed from that file at startup so the
    /// editor can never fail to have a theme - `Theme::default()` is infallible, and
    /// `theme::the_default_theme_is_the_undertow_file` is what keeps the two in
    /// step. It is also the fallback for any slot a loaded theme file leaves unset.
    fn default() -> Self {
        // Depth is carried by blue: each surface that comes forward gets a lighter,
        // bluer ground. Colors are explicit RGB, never named ANSI ones, which the
        // terminal remaps to its own palette and can render as low-contrast
        // light-on-light (the same reason `theme::color` accepts hex only).
        Self {
            text: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x15, 0x18, 0x23)),
            head_bar: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x11, 0x14, 0x1d))
                .add_modifier(Modifier::BOLD),
            // Undertow's two rules, both applied: the ground is lifted one step above
            // the bar (depth is carried by blue), and the accent it reserves for
            // "you are here" is spent on the text rather than flooding the strip. The
            // lift stops below `selection`, so the tab never comes forward of it.
            head_bar_active: Style::new()
                .fg(Color::Rgb(0x63, 0xd2, 0xc3))
                .bg(Color::Rgb(0x22, 0x28, 0x3c))
                .add_modifier(Modifier::BOLD),
            head_bar_inactive: Style::new()
                .fg(Color::Rgb(0x6b, 0x74, 0x96))
                .bg(Color::Rgb(0x11, 0x14, 0x1d)),
            head_bar_separator: Style::new()
                .fg(Color::Rgb(0x39, 0x40, 0x5e))
                .bg(Color::Rgb(0x11, 0x14, 0x1d)),
            status_bar: Style::new()
                .fg(Color::Rgb(0x8a, 0x93, 0xb5))
                .bg(Color::Rgb(0x11, 0x14, 0x1d)),
            gutter: Style::new().fg(Color::Rgb(0x4a, 0x52, 0x73)),
            gutter_current: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .add_modifier(Modifier::BOLD),
            selection: Style::new()
                .fg(Color::Rgb(0xee, 0xf1, 0xfa))
                .bg(Color::Rgb(0x2b, 0x35, 0x57)),
            current_line: Style::new().bg(Color::Rgb(0x1c, 0x20, 0x31)),
            // A step lighter than the current-line tint, so where the two cross the
            // ruler is still the thing you see (an overlay patches the row's base).
            ruler: Style::new().bg(Color::Rgb(0x21, 0x26, 0x3a)),
            // Quieter than the gutter's own numbers, which are already the dimmest
            // text on screen: a guide repeats on every row of every indented block,
            // so it has to sit below the threshold at which the eye stops on it.
            indent_guide: Style::new().fg(Color::Rgb(0x2f, 0x36, 0x50)),
            // The track sits at the gutter's weight and the thumb well above it: the
            // pair has to be separable at a glance from the far edge of the screen,
            // where nothing else is competing for the eye.
            scrollbar_track: Style::new().fg(Color::Rgb(0x2b, 0x31, 0x49)),
            scrollbar_thumb: Style::new().fg(Color::Rgb(0x5a, 0x64, 0x8c)),
            // A shade above the current-line tint: the header has to read as a
            // different *surface* from the text below it (it is the one place on
            // screen where a row is not the line it appears to be), while staying
            // far enough below the selection that pinned code is still code.
            sticky_context: Style::new().bg(Color::Rgb(0x23, 0x28, 0x3d)),
            // A violet block: the terminal has one real cursor, which the primary
            // caret uses, so the others need a color of their own (SPEC §2.2).
            secondary_cursor: Style::new()
                .fg(Color::Rgb(0x15, 0x18, 0x23))
                .bg(Color::Rgb(0x7d, 0x6c, 0xe0)),
            // Search (SPEC §11): amber, the one hue nothing else in the theme uses -
            // a match must not read as a selection or a diagnostic. The other matches
            // get a dim ground that leaves the syntax colors legible through it; the
            // current one gets the full fill, so "which one is next" is answerable at
            // a glance on a screen full of hits.
            search_match: Style::new().bg(Color::Rgb(0x54, 0x42, 0x1c)),
            search_current: Style::new()
                .fg(Color::Rgb(0x1a, 0x14, 0x08))
                .bg(Color::Rgb(0xe0, 0xa8, 0x3e)),
            // Toasts (SPEC §7.5): a sunk slate for info, a strong red for errors, so
            // a failure is unmistakable (SPEC §8: never silent).
            toast_info: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x22, 0x28, 0x3c)),
            toast_error: Style::new()
                .fg(Color::Rgb(0xff, 0xe7, 0xec))
                .bg(Color::Rgb(0x7a, 0x2f, 0x3d))
                .add_modifier(Modifier::BOLD),
            // The palette floats above the body, so it gets its own lighter panel;
            // the selection's blue marks the highlighted row (SPEC §7.5).
            palette: Style::new()
                .fg(Color::Rgb(0xcc, 0xd2, 0xe4))
                .bg(Color::Rgb(0x1a, 0x1e, 0x2c)),
            palette_selected: Style::new()
                .fg(Color::Rgb(0xee, 0xf1, 0xfa))
                .bg(Color::Rgb(0x2b, 0x35, 0x57))
                .add_modifier(Modifier::BOLD),
            // Diagnostics (SPEC §5): a red error and an amber warning carry the
            // usual severity signal, while information and hint stay quiet - a
            // desaturated blue and a muted grey - so a wall of hints never shouts
            // over a real error. These are the underline/gutter foregrounds.
            diagnostic_error: Style::new().fg(Color::Rgb(0xe0, 0x6c, 0x75)),
            diagnostic_warning: Style::new().fg(Color::Rgb(0xd6, 0x9d, 0x53)),
            diagnostic_information: Style::new().fg(Color::Rgb(0x61, 0x9a, 0xd6)),
            diagnostic_hint: Style::new().fg(Color::Rgb(0x7d, 0x86, 0xa8)),
            // Syntax (SPEC §5, M4): a restrained scheme on undertow's blue ground -
            // a violet keyword, a blue function, a warm-gold type, a green string, a
            // dim slate comment, an orange constant, the body color for variables so
            // ordinary identifiers stay calm, and a muted punctuation.
            syntax_keyword: Style::new().fg(Color::Rgb(0xb1, 0x8b, 0xe0)),
            syntax_function: Style::new().fg(Color::Rgb(0x61, 0x9a, 0xd6)),
            syntax_type: Style::new().fg(Color::Rgb(0xd6, 0xb2, 0x70)),
            syntax_string: Style::new().fg(Color::Rgb(0x8c, 0xc2, 0x65)),
            syntax_comment: Style::new()
                .fg(Color::Rgb(0x5a, 0x63, 0x82))
                .add_modifier(Modifier::ITALIC),
            syntax_constant: Style::new().fg(Color::Rgb(0xd6, 0x9d, 0x53)),
            syntax_variable: Style::new().fg(Color::Rgb(0xcc, 0xd2, 0xe4)),
            syntax_punctuation: Style::new().fg(Color::Rgb(0x8a, 0x93, 0xb5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- The config file (SPEC §10.5) -----------------------------------------

    #[test]
    fn an_empty_or_missing_config_is_the_built_in_default() {
        // A fresh install has no config file, which is not a problem to report.
        let (config, problem) = parse("");
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
        assert!(config.final_newline);
        assert_eq!(problem, None);

        let missing = std::path::Path::new("/nonexistent/vortex/config.toml");
        let (config, problem) = load(missing, false);
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
        assert_eq!(problem, None);

        // ...but one named with `--config` is a typo, and starting silently on the
        // defaults leaves the user with none of their settings and no reason given.
        let (config, problem) = load(missing, true);
        assert_eq!(config.theme_name, crate::theme::DEFAULT, "still starts");
        let problem = problem.expect("an explicit path that is not there is reported");
        assert!(problem.contains("no such config file"), "{problem}");
    }

    #[test]
    fn each_setting_can_be_given_on_its_own() {
        // Every key is optional, so a config file can be one line and everything
        // else keeps its default.
        let (config, problem) = parse("tab_width = 8");
        assert_eq!(problem, None);
        assert_eq!(config.tab_width, 8);
        assert!(config.final_newline, "untouched keys keep their default");

        let (config, problem) = parse("final_newline = false");
        assert_eq!(problem, None);
        assert!(!config.final_newline);
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);

        let (config, problem) = parse(r#"line_numbers = "relative""#);
        assert_eq!(problem, None);
        assert_eq!(config.line_numbers, LineNumbers::Relative);
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
    }

    #[test]
    fn rulers_are_a_list_and_default_to_none() {
        assert!(Config::default().rulers.is_empty(), "off unless asked");

        let (config, problem) = parse("rulers = [80, 100]");
        assert_eq!(problem, None);
        assert_eq!(config.rulers, vec![80, 100]);

        // An explicit empty list is a way to say "none", not a parse error.
        let (config, problem) = parse("rulers = []");
        assert_eq!(problem, None);
        assert!(config.rulers.is_empty());
    }

    #[test]
    fn indent_guides_are_off_unless_the_file_asks_for_them() {
        assert!(!Config::default().indent_guides, "off unless asked");

        let (config, problem) = parse("indent_guides = true");
        assert_eq!(problem, None);
        assert!(config.indent_guides);

        let (config, problem) = parse("indent_guides = false");
        assert_eq!(problem, None);
        assert!(!config.indent_guides);
    }

    #[test]
    fn the_scrollbar_is_on_until_the_file_declines_it() {
        // The one piece of chrome that is on by default: it marks no row, and it
        // answers a question nothing else on screen does - where the *window* sits in
        // the file, as against where the caret sits, which is the status bar's job.
        assert!(Config::default().scrollbar, "on unless declined");

        let (config, problem) = parse("scrollbar = false");
        assert_eq!(problem, None);
        assert!(!config.scrollbar, "and the column is buyable back");

        let (config, problem) = parse("scrollbar = true");
        assert_eq!(problem, None);
        assert!(config.scrollbar);
    }

    #[test]
    fn sticky_context_is_off_unless_the_file_asks_for_it() {
        // As strict as the scrollbar's default, and for the same kind of reason: this
        // one costs *rows* of text rather than a mark on cells the text was not using.
        assert!(!Config::default().sticky_context, "off unless asked");

        let (config, problem) = parse("sticky_context = true");
        assert_eq!(problem, None);
        assert!(config.sticky_context);

        let (config, problem) = parse("sticky_context = false");
        assert_eq!(problem, None);
        assert!(!config.sticky_context);
    }

    #[test]
    fn the_gutter_numbers_absolutely_unless_asked_otherwise() {
        assert_eq!(Config::default().line_numbers, LineNumbers::Absolute);
        let (config, problem) = parse(r#"line_numbers = "absolute""#);
        assert_eq!(problem, None);
        assert_eq!(config.line_numbers, LineNumbers::Absolute);
    }

    #[test]
    fn an_unknown_line_number_mode_is_reported_rather_than_ignored() {
        // A misspelled *value* has to fail as loudly as a misspelled key - both
        // leave the user staring at a setting that did not apply.
        let (config, problem) = parse(r#"line_numbers = "hybrid""#);
        assert_eq!(config.line_numbers, LineNumbers::Absolute);
        let problem = problem.expect("an unknown mode is reported");
        // serde names the bad value *and* lists the accepted ones, which is more
        // use than naming the key: the key is the part the user got right.
        assert!(problem.contains("hybrid"), "message: {problem}");
        assert!(problem.contains("absolute"), "message: {problem}");
    }

    #[test]
    fn the_theme_is_the_setting_that_makes_a_pick_outlast_the_session() {
        let (config, problem) = parse(r#"theme = "undertow""#);
        assert_eq!(problem, None);
        assert_eq!(config.theme_name, "undertow");
        assert_eq!(config.theme, crate::theme::load_named("undertow").unwrap());
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_ignored() {
        // The theme-file rule, for the same reason: a typo must be reported, or
        // the user stares at a setting that "did not apply" with nothing to go on.
        let (config, problem) = parse("tab_wdith = 8");
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
        let problem = problem.expect("a typo is reported");
        assert!(problem.contains("tab_wdith"), "message: {problem}");
    }

    #[test]
    fn a_broken_file_still_leaves_a_working_editor() {
        // Every failure here degrades to the default for the part that failed and
        // says why; none of them stops the editor coming up (SPEC §8).
        let (config, problem) = parse("this is not toml at all");
        assert_eq!(config.theme, Theme::default());
        assert!(problem.is_some());

        let (config, problem) = parse("tab_width = 0");
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH, "zero is not a width");
        assert!(problem.unwrap().contains("tab_width"));

        let (config, problem) = parse(r#"theme = "no-such-theme""#);
        assert_eq!(config.theme, Theme::default(), "colors are not optional");
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
        assert!(problem.unwrap().contains("no-such-theme"));
    }

    #[test]
    fn a_keys_table_is_layered_over_the_built_in_bindings() {
        use crate::command::Command;
        use crate::keymap::command_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (config, problem) = parse("[keys]\n\"ctrl+e\" = \"quit\"\n");
        assert_eq!(problem, None);
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_e, 10),
            Some(Command::Editor(vortex_core::Action::Quit))
        );
        // Rebinding one chord leaves the other fifty alone.
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_s, 10),
            Some(Command::Editor(vortex_core::Action::Save { force: false }))
        );
    }

    #[test]
    fn a_keys_subtable_binds_a_surface_and_reports_a_bad_context() {
        use crate::keymap::{Command as Bound, Context};
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // `[keys]` is still the editor's table and `[keys.picker]` is a context, so a
        // user can rebind a surface key the frontend used to hold in code (SPEC §10.5).
        let (config, problem) = parse("[keys.picker]\n\"ctrl+n\" = \"next_item\"\n");
        assert_eq!(problem, None);
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(
            config.keymap.bound(Context::Picker, ctrl_n),
            Some(Bound::NextItem)
        );
        // The contexts are a closed set and the commands are scoped, so a typo in
        // either is a complaint rather than a row that never fires (SPEC §8).
        let (_, problem) = parse("[keys.pickr]\n\"ctrl+n\" = \"next_item\"\n");
        assert!(problem.unwrap().contains("unknown key context"));
        let (_, problem) = parse("[keys]\n\"ctrl+n\" = \"next_item\"\n");
        assert!(
            problem.unwrap().contains("cannot be bound in the `editor`"),
            "a surface command in the editor table is reported"
        );
    }

    #[test]
    fn rebinding_a_bound_chord_replaces_it() {
        use crate::command::Command;
        use crate::keymap::command_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (config, problem) = parse("[keys]\n\"ctrl+s\" = \"quit\"\n");
        assert_eq!(problem, None);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_s, 10),
            Some(Command::Editor(vortex_core::Action::Quit))
        );
    }

    #[test]
    fn every_independent_complaint_is_reported_at_once() {
        // These fail independently, so overwriting meant three typos took three
        // restarts to find - each fix revealing the next one.
        let (config, problem) = parse(
            "tab_width = 0\ntheme = \"no-such-theme\"\n[keys]\n\"ctrl+e\" = \"frobnicate\"\n",
        );
        let problem = problem.expect("three things were wrong");
        assert!(problem.contains("tab_width"), "{problem}");
        assert!(problem.contains("no-such-theme"), "{problem}");
        assert!(problem.contains("frobnicate"), "{problem}");
        // ...and each part still degraded to its own default.
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
    }

    #[test]
    fn a_typo_in_one_binding_costs_only_that_binding() {
        use crate::command::Command;
        use crate::keymap::command_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        // `keys` arrives as a sorted table, so "ctrl+a" is applied before "ctrl+z"
        // whatever order they were written in. Stopping at the first bad one
        // therefore discarded a good binding written *above* the typo - the opposite
        // of what the doc promised. Every binding that parses is now applied.
        let (config, problem) = parse(
            "[keys]\n\"ctrl+z\" = \"quit\"\n\"ctrl+a\" = \"frobnicate\"\n\"ctrl+e\" = \"save\"\n",
        );
        let problem = problem.expect("the typo is still reported");
        assert!(problem.contains("frobnicate"), "{problem}");

        let bound = |c: char| {
            command_for_key(
                &config.keymap,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
                10,
            )
        };
        assert_eq!(
            bound('z'),
            Some(Command::Editor(vortex_core::Action::Quit)),
            "sorts after the typo, and survived it"
        );
        assert_eq!(
            bound('e'),
            Some(Command::Editor(vortex_core::Action::Save { force: false })),
            "sorts after the typo, and survived it"
        );
    }

    #[test]
    fn a_long_complaint_does_not_swallow_the_ones_after_it() {
        // Bounding the *joined* string instead of each part reads the same and loses
        // whichever complaints came last - the "three typos, three restarts" problem
        // again, in a narrower window. A theme name long enough to fill a row on its
        // own must still leave the binding error visible behind it.
        let long_name = "z".repeat(400);
        let (_, problem) = parse(&format!(
            "theme = \"{long_name}\"\n[keys]\n\"ctrl+e\" = \"frobnicate\"\n"
        ));
        let problem = problem.expect("both were wrong");
        assert!(
            problem.contains("frobnicate"),
            "the second complaint was cut off: {problem}"
        );
    }

    #[test]
    fn a_config_that_is_not_a_config_cannot_flood_the_toast() {
        // The toast is one row of one screen width. Every message here quotes the
        // file back in some form, so a binary pointed at with --config must not
        // carry a megabyte-long line into it.
        let huge = "x".repeat(50_000);
        for text in [
            format!("[keys]\n\"ctrl+e\" = \"{huge}\"\n"),
            format!("[keys]\n\"{huge}\" = \"quit\"\n"),
            format!("theme = \"{huge}\"\n"),
            format!("{huge} = 1\n"),
        ] {
            let (_, problem) = parse(&text);
            let problem = problem.expect("each of these is a complaint");
            assert!(problem.len() <= 220, "message was {} bytes", problem.len());
            assert!(!problem.contains('\n'), "message must be one line");
        }
    }

    #[test]
    fn a_binding_that_names_nothing_is_reported() {
        let (_, problem) = parse("[keys]\n\"ctrl+e\" = \"frobnicate\"\n");
        assert!(problem.unwrap().contains("frobnicate"));
        let (_, problem) = parse("[keys]\n\"ctrl+nonsense\" = \"quit\"\n");
        assert!(problem.unwrap().contains("nonsense"));
    }

    #[test]
    fn only_the_settings_the_core_acts_on_cross_the_seam() {
        // Tab width and the keymap are the frontend's business; the final-newline
        // policy is the core's, because the core is what writes the file.
        let config = Config {
            final_newline: false,
            tab_width: 8,
            ..Default::default()
        };
        assert!(!config.core_options().final_newline);
        assert!(Config::default().core_options().final_newline);
    }

    #[test]
    fn the_config_path_follows_xdg_before_home() {
        // Same rule as the themes directory, so one dotfiles repo works everywhere.
        use std::ffi::OsString;
        assert_eq!(
            config_path(
                Some(OsString::from("/xdg")),
                Some(OsString::from("/home/me"))
            ),
            Some(PathBuf::from("/xdg/vortex/config.toml")),
            "XDG wins when it is set"
        );
        assert_eq!(
            config_path(None, Some(OsString::from("/home/me"))),
            Some(PathBuf::from("/home/me/.config/vortex/config.toml"))
        );
        assert_eq!(
            config_path(None, None),
            None,
            "nowhere to look is not an error"
        );
        // And the live rule resolves without panicking, whatever this machine has.
        let _ = user_path();
    }

    #[test]
    fn default_config_carries_the_builtin_theme_and_its_name() {
        let config = Config::default();
        assert_eq!(config.theme, Theme::default());
        // The name must be a theme that actually resolves, or the picker opens with
        // nothing highlighted and a cancelled preview restores a theme that is gone.
        assert_eq!(config.theme_name, crate::theme::DEFAULT);
        assert_eq!(
            crate::theme::load_named(&config.theme_name).unwrap(),
            config.theme
        );
    }

    #[test]
    fn default_config_carries_a_working_keymap() {
        use crate::command::Command;
        use crate::keymap::command_for_key;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use vortex_core::Action;

        let config = Config::default();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_s, 10),
            Some(Command::Editor(Action::Save { force: false }))
        );
        // Overlay triggers ride the same table, so the resolved config carries them
        // too - the property that breaks if they are ever built outside `from_pairs`.
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(
            command_for_key(&config.keymap, ctrl_p, 10),
            Some(Command::OpenPalette)
        );
    }

    #[test]
    fn default_theme_pins_every_color_to_true_color() {
        // Named/indexed ANSI colors are remapped by the user's terminal profile, so
        // a theme built from them cannot promise the contrast it was designed with.
        // Every slot the built-in theme fills must therefore be `Color::Rgb`.
        let t = Theme::default();
        let slots = [
            ("text", t.text),
            ("head_bar", t.head_bar),
            ("head_bar_active", t.head_bar_active),
            ("head_bar_inactive", t.head_bar_inactive),
            ("head_bar_separator", t.head_bar_separator),
            ("status_bar", t.status_bar),
            ("gutter", t.gutter),
            ("gutter_current", t.gutter_current),
            ("selection", t.selection),
            ("current_line", t.current_line),
            ("secondary_cursor", t.secondary_cursor),
            ("toast_info", t.toast_info),
            ("toast_error", t.toast_error),
            ("palette", t.palette),
            ("palette_selected", t.palette_selected),
            ("diagnostic_error", t.diagnostic_error),
            ("diagnostic_warning", t.diagnostic_warning),
            ("diagnostic_information", t.diagnostic_information),
            ("diagnostic_hint", t.diagnostic_hint),
            ("syntax_keyword", t.syntax_keyword),
            ("syntax_function", t.syntax_function),
            ("syntax_type", t.syntax_type),
            ("syntax_string", t.syntax_string),
            ("syntax_comment", t.syntax_comment),
            ("syntax_constant", t.syntax_constant),
            ("syntax_variable", t.syntax_variable),
            ("syntax_punctuation", t.syntax_punctuation),
        ];
        for (name, style) in slots {
            for color in [style.fg, style.bg].into_iter().flatten() {
                assert!(matches!(color, Color::Rgb(..)), "{name}: {color:?}");
            }
        }
        // The body has both a ground and an ink, so the theme owns the whole surface.
        assert!(t.text.fg.is_some() && t.text.bg.is_some());
        assert!(t.gutter_current.add_modifier.contains(Modifier::BOLD));
        assert!(t.head_bar.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn highlight_maps_every_kind_to_its_role() {
        use vortex_core::HighlightKind as K;
        let t = Theme::default();
        // A representative kind from each of the eight roles resolves to that role's
        // slot; related kinds share a role (a macro paints as a function).
        assert_eq!(t.highlight(K::Keyword), t.syntax_keyword);
        assert_eq!(t.highlight(K::Function), t.syntax_function);
        assert_eq!(t.highlight(K::Macro), t.syntax_function);
        assert_eq!(t.highlight(K::Type), t.syntax_type);
        assert_eq!(t.highlight(K::TypeBuiltin), t.syntax_type);
        assert_eq!(t.highlight(K::String), t.syntax_string);
        assert_eq!(t.highlight(K::Escape), t.syntax_string);
        assert_eq!(t.highlight(K::Comment), t.syntax_comment);
        assert_eq!(t.highlight(K::ConstantBuiltin), t.syntax_constant);
        assert_eq!(t.highlight(K::Parameter), t.syntax_variable);
        assert_eq!(t.highlight(K::Punctuation), t.syntax_punctuation);
        assert_eq!(t.highlight(K::Attribute), t.syntax_punctuation);
    }
}

#[cfg(test)]
mod readme_tests {
    /// The README documents the config format with a worked example. A format doc
    /// that does not parse is worse than none, so it is held to the real parser -
    /// the same guard the theme format carries.
    #[test]
    fn the_readme_example_config_parses() {
        let readme = include_str!("../../../README.md");
        let block = readme
            .split("```toml")
            .find(|b| b.contains("final_newline"))
            .and_then(|rest| rest.split("```").next())
            .expect("README documents the config format in a toml block");
        let (config, problem) = super::parse(block);
        assert_eq!(problem, None, "the README's example config must load");
        // ...and it must actually be the example, not an empty parse that says yes.
        assert_eq!(config.theme_name, "phosphor");
        assert_eq!(config.tab_width, 4);
    }
}
