//! `vortex-tui` - the terminal frontend (binary `vortex`).
//!
//! A **thin** frontend (SPEC §1, §7): it owns the executor, spawns the core actor,
//! translates keys to `Action`s (via [`keymap`], tested), and paints from the
//! latest `ViewSnapshot` (viewport math in [`layout`], tested). All editing logic
//! lives in the core; this file is the untestable I/O shell - raw-mode setup, the
//! `event::read` loop, and the ratatui draw call - kept as small as possible.
//!
//! Rendering (SPEC §5, §7): we own the loop; ratatui already cell-diffs, so there
//! is no custom renderer. Each frame is wrapped in synchronized-output
//! (`BeginSynchronizedUpdate`/`EndSynchronizedUpdate`) so a terminal never paints a
//! half-written frame (anti-tearing). The Kitty keyboard protocol is negotiated at
//! startup for rich modifiers (SPEC §9), with graceful fallback where unsupported.

// The frontend's logic modules live in the library crate (see `lib.rs`); the
// binary depends on them. Only `testutil` is redeclared here: it is `cfg(test)`,
// and a dependency's test-only items are invisible to the crate depending on it,
// so the binary's own tests need their own view of the same file.
#[cfg(test)]
#[path = "testutil.rs"]
mod testutil;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, supports_keyboard_enhancement,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr;

use vortex_core::{Action, BufferId, BufferInfo, Core, Granularity, ViewSnapshot};

use vortex_tui::command::{self, Command};
use vortex_tui::compositor::{Compositor, EventResult};
use vortex_tui::toast::{self, Toasts};
use vortex_tui::{
    bufferpicker, buffersearch, click, config, filepicker, formatpicker, globalsearch, grammar,
    keymap, layout, osc52, palette, prompt, theme, themepicker, watcher,
};

/// The frontend's view state: which window of the buffer is on screen. Both axes
/// are pure frontend concerns (SPEC §5) - scrolling reads a different window of the
/// same snapshot with no core round-trip. Carried as one struct (not a growing
/// list of positional args) through the paint path, and updated by paint so the
/// caller can carry it to the next frame. (Named `ViewState` to avoid colliding
/// with ratatui's own `Viewport` type used in terminal setup.)
#[derive(Debug, Clone, Copy, Default)]
struct ViewState {
    /// Index of the top visible line (vertical scroll).
    scroll: usize,
    /// Leftmost visible display column (horizontal scroll).
    h_scroll: usize,
    /// Text rows the last frame showed - the basis for the PageUp/PageDown step.
    /// 0 before the first paint.
    page_height: usize,
}

impl ViewState {
    /// Lines a PageUp/PageDown moves the cursor: one screenful less a line of
    /// context overlap, at least 1 so a tiny or not-yet-painted viewport still
    /// moves.
    fn page(&self) -> usize {
        self.page_height.saturating_sub(1).max(1)
    }
}

/// How long the input poll blocks before we tick the render loop anyway, so a
/// snapshot that arrives without a keystroke (e.g. a background restyle in M4)
/// still gets painted promptly.
const POLL: Duration = Duration::from_millis(16);

/// How long the first paint of a freshly-opened highlightable buffer waits for its
/// syntax highlights, so the file appears already colored instead of flashing plain
/// text for a frame first (M4). Highlights normally arrive within a frame or two of
/// the text (the grammar is pre-warmed), so this deadline only bounds the wait for a
/// slow parse or a missing grammar - a brief hold, never a stall.
const HIGHLIGHT_WAIT: Duration = Duration::from_millis(150);

/// Lines the mouse wheel scrolls the viewport per notch. A few lines per notch is
/// the common terminal feel; scrolling is a pure frontend viewport move (SPEC §5),
/// so it never round-trips to the core.
const SCROLL_STEP: usize = 3;

fn main() -> io::Result<()> {
    // Parse argv before touching the terminal: `--help`/`--version` and bad flags
    // must print to normal stdout/stderr, not paint into the alternate screen.
    let (path, config_path) = match parse_args(std::env::args_os().skip(1)) {
        Args::Open { file, config } => (file, config),
        Args::MissingValue(flag) => {
            eprintln!("vortex: '{flag}' needs a value\n{USAGE}");
            std::process::exit(2);
        }
        Args::Help => {
            print!("{HELP}{UNDO_REDO_HELP}");
            return Ok(());
        }
        Args::Version => {
            println!("vortex {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Args::Unknown(flag) => {
            eprintln!(
                "vortex: unknown option '{}'\n{USAGE}",
                flag.to_string_lossy()
            );
            // Exit directly with the conventional "usage" code rather than
            // returning Err, which would print a second, redundant Rust-formatted
            // error line after our own message.
            std::process::exit(2);
        }
    };

    // The core is a single-owner actor task (SPEC §2.3). It runs on its OWN thread
    // via `block_on`, and the frontend talks to it only over channels - never a
    // shared method call. This split is load-bearing, not incidental: the frontend
    // does *blocking* terminal I/O (`event::poll`/`read`), which would starve a
    // single-threaded executor shared with the core and freeze the actor. Because
    // the seam is message-passing, giving each side its own thread is a threading
    // change with zero logic change.
    let Core { handle, run } = vortex_core::new(1024);
    let core_thread = std::thread::Builder::new()
        .name("vortex-core".into())
        .spawn(move || smol::block_on(run))?;

    // Watch the open files for changes made outside the editor (SPEC §10.2). One
    // watcher for the session, attached up front rather than lazily: it has no
    // per-language cost to defer, and the core announces every already-open file to
    // whatever attaches, so there is nothing to be early for. Its loop runs on its
    // own thread, off the render thread, like the other producers. A backend that
    // will not start is not fatal - the session simply does not notice external
    // changes, which is where every session was before this existed (SPEC §8).
    match watcher::watcher() {
        Ok((watch_handle, watch_loop)) => {
            std::thread::Builder::new()
                .name("vortex-watch".into())
                .spawn(move || smol::block_on(watch_loop))?;
            let _ = handle.watch.send_blocking(watch_handle);
        }
        Err(_) => {
            // Nothing to report to yet - the terminal is not even in raw mode, and
            // a message here would be scrolled away by the alternate screen.
        }
    }

    // Language servers are attached lazily, driven by the core's `FileOpened`
    // notification (SPEC §3, M2): whenever a file is opened - at launch, or via the
    // Ctrl+O picker - the frontend attaches a server for its type if one exists and
    // is not already running. A missing server degrades silently to no diagnostics
    // (SPEC §8). Each client's loop runs on its own thread, off the render thread,
    // preserving the no-starvation property above.
    let mut lsp = LspManager::new();
    // Syntax highlighters are attached the same way (M4): on each file open, the
    // frontend loads the file type's grammar and hands the core a highlighter.
    // Missing grammar degrades silently to no highlighting (SPEC §8).
    let mut grammars = GrammarManager::new();
    // Pre-warm the launch file's grammar now, so its ~200ms load runs in the
    // background *during* terminal setup and the core's first read - by the time the
    // buffer opens and paints, the highlighter is attached and its first batch lands
    // with the text instead of a visible beat later. A later `FileOpened` for the
    // same file is deduplicated inside `ensure`.
    if let Some(p) = &path {
        grammars.ensure(p, &handle);
    }

    // Resolve configuration once, up front - next to argv, because that is where
    // `--config` arrives and because it must be settled before the first frame
    // paints (SPEC §10.5). A file that will not parse degrades to the defaults and
    // reports itself as a toast below, once there is a screen to show it on: the
    // editor starting is not negotiable (SPEC §8).
    let (config, config_problem) = match config_path.or_else(config::user_path) {
        Some(path) => config::load(&path),
        // No home directory to look in: built-in defaults, and nothing to report.
        None => (config::Config::default(), None),
    };
    // Settings the core acts on rather than the frontend (SPEC §10.5). Sent before
    // the first file is opened so the very first save already honors them.
    let _ = handle
        .actions
        .send_blocking(vortex_core::Action::Configure(config.core_options()));

    // Terminal setup. On any error we still attempt teardown so we never leave the
    // user's terminal in raw mode (the Drop impl is the backstop).
    let mut term = TerminalGuard::enter()?;
    let result = event_loop(
        &handle,
        &mut term.terminal,
        path,
        config,
        config_problem,
        &mut lsp,
        &mut grammars,
    );
    term.leave();

    // Dropping the handle closes the action channel, so the core loop ends; join
    // it so the process does not exit while the actor is mid-shutdown.
    drop(handle);
    let _ = core_thread.join();
    result
}

/// Attaches language servers to the core on demand and remembers which are already
/// running, so a server is launched at most once per (command, workspace root).
///
/// Attachment is lazy: the first open of a file type that has a server (and whose
/// server is installed) launches it; later opens of the same type reuse it, since
/// the core announces every opened file to the attached server (a `didOpen`). This
/// is why opening a `.rs` file with Ctrl+O gets diagnostics even when the editor
/// was launched on nothing or on a non-Rust file.
struct LspManager {
    /// The (command, root) pairs already attached, to avoid relaunching a server
    /// that already covers this file's workspace.
    attached: std::collections::HashSet<(&'static str, PathBuf)>,
}

impl LspManager {
    fn new() -> Self {
        Self {
            attached: std::collections::HashSet::new(),
        }
    }

    /// Ensure a language server covers `path`, attaching one if the file type has a
    /// server, it is installed, and it is not already running for this workspace.
    ///
    /// The client's loop is spawned on its own thread (off the render thread), and
    /// the handle is handed to the core, which swaps it in and announces the
    /// current buffer to it. A missing server, or a send to a stopped core, is
    /// ignored - the editor keeps running with no diagnostics (SPEC §8).
    fn ensure(&mut self, path: &Path, handle: &vortex_core::CoreHandle) {
        let Some((command, root)) = lsp_target(path) else {
            return;
        };
        // `insert` returns false when the pair was already present: the server for
        // this workspace is running, so the core's own `didOpen` covers the file.
        if !self.attached.insert((command, root.clone())) {
            return;
        }
        let (lsp_handle, lsp_loop) = vortex_core::lsp::client(command, &root);
        // The loop resolves to why it stopped; a spawn/protocol failure is
        // swallowed rather than crashing the editor (SPEC §8). Surfacing it as a
        // toast is a later refinement.
        let spawned = std::thread::Builder::new()
            .name("vortex-lsp".into())
            .spawn(move || {
                let _ = smol::block_on(lsp_loop);
            });
        if spawned.is_err() {
            self.attached.remove(&(command, root));
            return;
        }
        // A closed channel means the core has stopped; nothing to attach to.
        let _ = handle.lsp.send_blocking(lsp_handle);
    }
}

/// Attaches syntax highlighters to the core on demand and remembers the language
/// currently attached, so it neither reloads a grammar for a same-language open nor
/// leaves the wrong one attached when the file's language changes.
///
/// The syntax twin of [`LspManager`], driven off the same `FileOpened`
/// notification. The resolution it needs (which library, which queries) is decided
/// in [`grammar`]; this owns only the `dlopen`-and-attach I/O, kept here beside the
/// LSP glue because it is the same shape and equally untestable.
struct GrammarManager {
    /// The language whose highlighter is currently attached, if any. Keyed by
    /// language (not workspace, unlike LSP) because a grammar is global: opening
    /// another file of the same language reuses the running highlighter, while a
    /// different language replaces it in the core.
    current: Option<&'static str>,
    /// Languages whose grammar has already been loaded, and the `Language` handle
    /// each one produced. Shared with the loader threads.
    ///
    /// Without this, switching between two buffers of different languages would
    /// `dlopen` a grammar every time focus moved (M7 made that a per-keystroke-ish
    /// event rather than a once-per-session one), paying ~200ms *and* leaking the
    /// library image on each pass, since `load_grammar` deliberately never unmaps.
    /// A `Language` is a pointer into an image that stays mapped for the process, so
    /// caching it is sound by construction and re-attaching becomes near-free.
    loaded: Arc<Mutex<HashMap<&'static str, tree_sitter::Language>>>,
    /// Bumped once per attach, so a loader thread can tell on finishing whether it is
    /// still the attach the editor wants.
    ///
    /// Loads race: they run on their own threads and take wildly different times - a
    /// cold `dlopen` is ~200ms while a cached one returns at once. Switching
    /// rust -> go -> rust inside that window would otherwise let the slow Go load land
    /// *after* the fast cached Rust one and replace it, leaving the core parsing a
    /// `.rs` file with the Go grammar. Nothing would correct it either: `current`
    /// already reads `rust`, so no further switch to Rust re-attaches, and the spans
    /// carry the right `buffer_id`/`version` so the core accepts them. The file just
    /// stays wrong.
    generation: Arc<AtomicU64>,
}

impl GrammarManager {
    fn new() -> Self {
        Self {
            current: None,
            loaded: Arc::new(Mutex::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Ensure the highlighter attached to the core matches `path`'s language,
    /// loading and attaching its grammar if it differs from the current one. A file
    /// type with no grammar, a missing library, or a load failure leaves the editor
    /// running with no fresh highlights (SPEC §8) - never crashing.
    ///
    /// **The whole load runs on a background thread.** Resolving the grammar,
    /// `dlopen`ing it, and compiling its queries take ~200ms; doing that on the
    /// render thread would stall the first frames right when the buffer opens. So
    /// the thread does the load, hands the core the highlighter, and then runs its
    /// loop - the render thread never blocks. `current` is set *before* spawning, so
    /// a language is loaded at most once even though the launch file is both
    /// pre-warmed (see `main`) and re-announced via `FileOpened`.
    ///
    /// Called on a buffer *switch* as well as an open (M7): the highlighter holds one
    /// grammar, so moving focus to a buffer of another language has to re-attach.
    /// That is what makes the grammar cache load-bearing rather than an optimization -
    /// alternating between two languages would otherwise reload on every switch.
    /// Returns whether an attach was actually started, which is the caller's signal
    /// that a fresh highlight batch is on its way and worth briefly waiting for.
    fn ensure(&mut self, path: &Path, handle: &vortex_core::CoreHandle) -> bool {
        let Some(lang) = grammar::grammar_target(path) else {
            return false;
        };
        // Same language as the running (or already-loading) highlighter: its resync
        // covers the newly opened file, so do not load the grammar again.
        if self.current == Some(lang) {
            return false;
        }
        self.current = Some(lang);
        // Claim this attach. A loader that finishes after a later one started is
        // stale and must not overwrite it (see `generation`).
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // Only the syntax-attach sender and the shared state cross to the thread (all
        // cheap clones); the render loop keeps the receivers.
        let syntax_tx = handle.syntax.clone();
        let loaded = Arc::clone(&self.loaded);
        let generation = Arc::clone(&self.generation);
        let _ = std::thread::Builder::new()
            .name("vortex-syntax".into())
            .spawn(move || {
                let Some(resolved) = grammar::resolve(lang) else {
                    return;
                };
                // Reuse an already-mapped grammar. A poisoned lock (a loader thread
                // panicked) is not worth taking the editor down for: fall back to
                // loading it again, which is correct, just slower.
                let cached = loaded.lock().ok().and_then(|m| m.get(lang).cloned());
                let language = match cached {
                    Some(language) => language,
                    None => {
                        let Some(language) = load_grammar(&resolved.lib_path) else {
                            return;
                        };
                        if let Ok(mut map) = loaded.lock() {
                            map.insert(lang, language.clone());
                        }
                        language
                    }
                };
                // Another attach started while this one was loading, so it owns the
                // highlighter now: drop this grammar rather than replace what the
                // editor is currently using with a language it has moved off.
                if generation.load(Ordering::SeqCst) != mine {
                    return;
                }
                let (syntax_handle, syntax_loop) = vortex_core::highlighter(
                    language,
                    lang,
                    resolved.highlights,
                    resolved.injections,
                    String::new(),
                );
                // A closed channel means the core has stopped; nothing to attach to.
                // Otherwise run the highlighter loop here until the core drops it.
                if syntax_tx.send_blocking(syntax_handle).is_ok() {
                    let _ = smol::block_on(syntax_loop);
                }
            });
        true
    }
}

/// Load a grammar library and return its `Language`, or `None` if it cannot be
/// opened or does not export the grammar entry point.
///
/// The library is deliberately leaked (`std::mem::forget`): the `Language` it
/// yields is a pointer into the library's image and must stay mapped for as long as
/// any highlighter thread uses it, which is the whole session, so leaking it for the
/// process lifetime is the simplest correct choice (and avoids a shutdown race
/// between unloading and the still-live highlighter thread).
fn load_grammar(lib_path: &Path) -> Option<tree_sitter::Language> {
    // SAFETY: `Library::new` runs the library's initializers; we load only grammar
    // dylibs resolved from the runtime/executable directories (trusted install
    // locations), and treat any failure as "no highlighting" rather than trusting
    // partially-loaded state.
    let lib = unsafe { libloading::Library::new(lib_path) }.ok()?;
    // SAFETY: the grammar contract is that a grammar dylib exports `vortex_grammar`
    // with exactly this ABI - `unsafe extern "C" fn() -> *const ()` returning its
    // static language pointer (see the `grammar-rust` crate). A file that does not
    // is rejected via `ok()?`.
    let language: tree_sitter::Language = unsafe {
        let entry: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
            lib.get(b"vortex_grammar").ok()?;
        tree_sitter_language::LanguageFn::from_raw(*entry).into()
    };
    // Keep the grammar mapped for the process; `language` borrows its image.
    std::mem::forget(lib);
    Some(language)
}

/// The language server and workspace root for a file, if one is known and
/// installed. Extension -> server is a small built-in table (only `rust-analyzer`
/// today, the M2 target); the root is the current working directory, where a
/// project's manifest lives when the editor is launched from its root. A per-file
/// root walk (nearest `Cargo.toml`) is a refinement, not needed for M2.
fn lsp_target(path: &Path) -> Option<(&'static str, PathBuf)> {
    let command = match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust-analyzer",
        _ => return None,
    };
    // Only report a server that is actually installed; probing here keeps the
    // "missing server is silent" contract in one place.
    if !server_on_path(command) {
        return None;
    }
    let root = std::env::current_dir().ok()?;
    Some((command, root))
}

/// Whether `command` resolves to an executable on the PATH. A cheap `--version`
/// probe: it must not paint into the alternate screen, so it runs before terminal
/// setup and discards all output.
fn server_on_path(command: &str) -> bool {
    std::process::Command::new(command)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

const USAGE: &str = "Usage: vortex [OPTIONS] [FILE]";
const HELP: &str = "\
Usage: vortex [OPTIONS] [FILE]

A terminal text editor. Opens FILE, or an empty buffer if omitted.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
      --config P   Read settings from P instead of the default config file
      --           Treat every following argument as a file name

Config:
  $XDG_CONFIG_HOME/vortex/config.toml, else ~/.config/vortex/config.toml
    theme = \"undertow\"   tab_width = 4   final_newline = true
    [keys]                 # chord = command, layered over the defaults
    \"ctrl+w\" = \"close_buffer\"

Keys:
  Ctrl+S           Save        Ctrl+Q            Quit
  Ctrl+Shift+S     Save as (prompt for a path; needs a Kitty-protocol terminal)
  Ctrl+O           Open file (fuzzy picker, previewing the highlighted file)
  Ctrl+F           Search the project (regex; Enter opens the file at the match)
  Ctrl+P           Command palette (type to filter, Enter runs, Esc cancels)
  Ctrl+T           Theme picker (previews as you move, Esc restores)
  Ctrl+Alt+Up/Down Add cursor above/below        Alt+Click  Add cursor
  Esc              Collapse to one cursor
";

/// The OS-conditional key lines of the help - undo/redo and clipboard - on the
/// platform's command modifier (Cmd on macOS, Ctrl elsewhere), matching [`keymap`]'s
/// OS-conditional bindings. On macOS Ctrl+C stays Quit (copy is Cmd+C); elsewhere
/// Ctrl+C is Copy and Quit is Ctrl+Q only. Split out because a `const` string cannot
/// be built per-OS by concatenation.
#[cfg(target_os = "macos")]
const UNDO_REDO_HELP: &str = "  Cmd+Z            Undo        Cmd+Y             Redo
  Cmd+C / X / V    Copy / Cut / Paste           Ctrl+C     Quit
";
#[cfg(not(target_os = "macos"))]
const UNDO_REDO_HELP: &str = "  Ctrl+Z           Undo        Ctrl+Y            Redo
  Ctrl+C / X / V   Copy / Cut / Paste
";

/// The outcome of parsing the command line - what `main` should do next.
#[derive(Debug, PartialEq, Eq)]
enum Args {
    /// Open this file, or start an empty unnamed buffer (`None`), reading settings
    /// from an explicit `--config` path when one was given.
    Open {
        file: Option<PathBuf>,
        config: Option<PathBuf>,
    },
    Help,
    Version,
    /// A flag that takes a value, with none following it.
    MissingValue(&'static str),
    /// An unrecognized `-`/`--` flag; report it rather than opening a file by
    /// that name (so `vortex --version` prints a version, not a "--version" buffer).
    Unknown(OsString),
}

/// Parse the argument list (already skipping argv[0]). The first positional
/// argument is the file to open; recognized flags map to help/version; an
/// unrecognized dashed argument is an error. `--` ends flag parsing so a file
/// literally named `--foo` is still openable. Pure and `OsString`-based (paths
/// need not be UTF-8) so it is unit-testable without a process (SPEC §13).
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Args {
    let mut file: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut flags_done = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if flags_done {
            file.get_or_insert_with(|| PathBuf::from(&arg));
            continue;
        }
        match arg.to_str() {
            Some("--") => flags_done = true,
            Some("-h" | "--help") => return Args::Help,
            Some("-V" | "--version") => return Args::Version,
            // `--config PATH`: the config is read before the first frame, so the
            // flag has to be here rather than being something the editor learns
            // later (SPEC §10.5).
            Some("--config") => match args.next() {
                Some(path) => config = Some(PathBuf::from(path)),
                None => return Args::MissingValue("--config"),
            },
            Some(s) if s.starts_with("--config=") => {
                config = Some(PathBuf::from(&s["--config=".len()..]));
            }
            // A dashed token we do not recognize (but not a lone "-", which is a
            // conventional stdin placeholder / valid-ish name, left as a path).
            Some(s) if s.starts_with('-') && s != "-" => return Args::Unknown(arg),
            // First positional wins; extra files are ignored until multi-buffer.
            _ => {
                file.get_or_insert_with(|| PathBuf::from(&arg));
            }
        }
    }
    Args::Open { file, config }
}

/// The render + input loop, run synchronously on the main thread. Returns when the
/// user quits or a channel closes. Uses blocking channel ops (`send_blocking`)
/// against the core running on its own thread; painting is driven by whichever
/// comes first each tick - an input event or the poll timeout, so a snapshot that
/// arrives without a keystroke (e.g. a background restyle in M4) still paints.
fn event_loop(
    handle: &vortex_core::CoreHandle,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    path: Option<PathBuf>,
    mut config: config::Config,
    // What went wrong reading the config file, if anything. Carried in rather than
    // reported where it was found, because the config resolves before the terminal
    // exists and a message printed then would be scrolled away by the alternate
    // screen. Surfaced as the first toast instead.
    config_problem: Option<String>,
    lsp: &mut LspManager,
    grammars: &mut GrammarManager,
) -> io::Result<()> {
    // Prime the view: open the CLI-given file, or just request a snapshot of the
    // empty buffer when none was given. Either way a snapshot follows, so the
    // first frame paints. Surface a failed prime (core thread never started)
    // rather than sitting on a blank screen forever.
    let prime = match path {
        Some(p) => Action::Open(p),
        None => Action::RequestSnapshot,
    };
    if handle.actions.send_blocking(prime).is_err() {
        return Ok(());
    }
    let mut latest: Option<ViewSnapshot> = None;
    // The active selection's grapheme count for the status bar. O(selected bytes)
    // to compute, so it is derived once per snapshot here and carried across
    // repaints - a toast tick or overlay keystroke must not re-walk a large
    // selection just to redraw the bar.
    let mut selected = 0;
    // View state (scroll on both axes + last page height). Updated by `draw` each
    // frame and carried forward; `page()` sizes PageUp/PageDown (SPEC §5).
    let mut viewport = ViewState::default();
    // Where each buffer was scrolled to, so switching away and back returns to the
    // same place instead of snapping to the top. Frontend-owned: the viewport is not
    // core state (SPEC §5), so the core neither knows nor needs to know about this.
    //
    // Only written when focus leaves a buffer, and dropped on the core's own
    // `BufferClosed`, so it neither costs a map write per frame nor grows without
    // bound. The live `viewport` below is always the active buffer's, so there is
    // nothing to store for it until it stops being active.
    let mut viewports: HashMap<BufferId, ViewState> = HashMap::new();
    // The buffer the last painted frame belonged to, so a switch can be noticed.
    let mut painted: Option<BufferId> = None;
    // Consecutive presses, so a double- or triple-click can be told from two
    // separate ones - the terminal reports presses and leaves the gesture to us.
    let mut clicks = click::Clicks::new();
    // Transient file/edit notices (open/save results, failures) surface here as
    // top-right toasts that auto-fade, rather than hijacking the status bar (SPEC
    // §7.5). A failed save must be visible, not silent (SPEC §8).
    let mut toasts = Toasts::new(config.theme.toast_info, config.theme.toast_error);
    // The first thing on screen when the config file had something wrong with it:
    // the editor came up on defaults, and this is what says why (SPEC §8).
    if let Some(problem) = config_problem {
        toasts.push(
            format!("Config: {problem}"),
            toast::Level::Error,
            Instant::now(),
        );
    }
    // Repaint only when something changed - a new snapshot, a resize, or the
    // first frame. Redrawing every idle poll tick is wasted work (ratatui
    // cell-diffs, so it emits nothing, but it still rebuilds the frame ~60x/sec).
    let mut needs_redraw = true;
    // Whether the next paint pulls the viewport to keep the caret visible. True for
    // every paint except one driven by a wheel scroll, which moves the view *away*
    // from the caret on purpose (SPEC §5, frontend-owned viewport); it resets to
    // true after each frame so a later edit/motion re-centers the caret.
    let mut follow = true;
    // The overlay UI stack (SPEC §7.5): empty while editing, holding a prompt/
    // palette/picker when one is open. Overlays get first refusal on keys and paint
    // over the base editor; an empty stack is a no-op on the hot path.
    let mut overlays = Compositor::new();
    // The live search (SPEC §11): empty while nothing is being searched for, holding
    // the query whose matches the body paints and whose pattern find-next repeats.
    // Frontend state, never core state - the preview it drives crosses no seam.
    let mut search = buffersearch::SearchState::default();
    // Set when a highlightable buffer has just opened but not yet painted: the first
    // paint is held until its highlights arrive (or `HIGHLIGHT_WAIT` elapses) so the
    // file never flashes plain-then-colored. Any input cancels the hold - a keystroke
    // must never wait on highlighting.
    let mut awaiting_highlight: Option<Instant> = None;

    loop {
        // Take the newest snapshot if the core published one (latest-wins cell).
        if let Some(snap) = handle.snapshots.try_recv() {
            selected = layout::selected_grapheme_count(&snap.text, &snap.selections);
            latest = Some(snap);
            needs_redraw = true;
        }
        // Drain the delta channel so its bounded buffer never fills; a local
        // terminal frontend paints from the snapshot, using deltas only as a
        // future partial-repaint hint (SPEC §5, §6).
        while handle.deltas.try_recv().is_ok() {}
        // Drain notifications: the bounded channel must not fill (every save emits
        // one, SPEC §6), and a file open/save result is surfaced in the status bar
        // (SPEC §8). Keep the latest message worth showing.
        while let Ok(note) = handle.notifications.try_recv() {
            // A newly opened file may want a language server (SPEC §3, M2): attach
            // one lazily for its type, whether the open came from argv or the
            // picker. Keyed off the core's own `FileOpened` so there is one path for
            // every open, and it fires with the path the core actually loaded.
            // A buffer arriving on screen may want a language server and a grammar
            // (SPEC §3, M2/M4). Both a fresh open and a *switch* qualify: the
            // highlighter holds one grammar at a time, so moving focus to a buffer of
            // another language has to re-attach exactly as an open does. Keyed off the
            // core's own notifications so there is one path for every way a buffer can
            // become current, carrying the path the core actually has.
            // An open always re-parses (the core marks the freshly loaded buffer
            // dirty), so colours are certainly coming. A switch does not: it leaves
            // the buffer's existing decorations in place deliberately, so a batch
            // arrives only when the language changed and a grammar was attached.
            let arriving: Option<(&Path, bool)> = match &note {
                vortex_core::Notification::FileOpened { path, .. } => Some((path, true)),
                vortex_core::Notification::BufferSwitched {
                    path: Some(path), ..
                } => Some((path, false)),
                _ => None,
            };
            if let Some((path, reparse_certain)) = arriving {
                lsp.ensure(path, handle);
                let attached = grammars.ensure(path, handle);
                // Hold the first paint for the colours only when a batch is genuinely
                // on its way. Arming it on a same-language switch would stall the
                // frame for the whole `HIGHLIGHT_WAIT` waiting for something nothing
                // was going to send - visible on any buffer that has no decorations
                // yet, such as an empty file.
                if (reparse_certain && grammar::grammar_target(path).is_some()) || attached {
                    awaiting_highlight = Some(Instant::now());
                }
            }
            // The core refused to close a buffer with unsaved work (SPEC §8). Ask, and
            // re-send the close forced if the user accepts - the confirmation is
            // frontend-local right up to the committed answer (SPEC §7.5).
            if let vortex_core::Notification::CloseRejected { buffer_id, path } = &note {
                overlays.push(prompt::confirm_close(
                    &config.theme,
                    *buffer_id,
                    path.as_deref(),
                ));
                needs_redraw = true;
            }
            // The file changed under a buffer that has unsaved work (SPEC §10.2).
            // Only the conflict gets a question - a clean buffer was already
            // reloaded by the core, and a *removed* file has nothing to reload
            // from, so both of those surface as a toast instead.
            if let vortex_core::Notification::ExternalChange {
                buffer_id,
                path,
                removed: false,
            } = &note
            {
                overlays.push(prompt::confirm_reload(
                    &config.theme,
                    *buffer_id,
                    Some(path.as_path()),
                ));
                needs_redraw = true;
            }
            // The buffer is gone, so its parked scroll position is too. Keyed off the
            // core's own event rather than inferred from the list shrinking.
            if let vortex_core::Notification::BufferClosed { buffer_id } = &note {
                viewports.remove(buffer_id);
            }
            // A copy/cut asks us to mirror the register to the OS clipboard. We push
            // it over OSC 52 (clipboard-over-terminal), which works locally and over
            // SSH (SPEC §11) without a native-clipboard dependency. Best-effort: a
            // terminal that ignores OSC 52 just leaves the OS clipboard unchanged.
            if let vortex_core::Notification::SetClipboard { text } = &note {
                let _ = osc52::copy(text);
            }
            if let Some((text, level)) = toast::toast_for(&note) {
                toasts.push(text, level, Instant::now());
                needs_redraw = true;
            }
        }
        // Fade toasts past their TTL. The 16ms poll tick below drives this even while
        // the user is idle, so a notice disappears on its own (SPEC §7.5).
        if toasts.expire(Instant::now()) {
            needs_redraw = true;
        }
        // Let the overlays take in whatever their own work has produced - the
        // global-search picker's results arrive on a channel, and the same idle tick
        // is what makes them appear while you wait rather than on the next keystroke.
        if overlays.tick() {
            needs_redraw = true;
        }

        // Hold the first paint of a just-opened highlightable buffer until its
        // highlights land, so it appears already colored rather than flashing plain
        // text first. The hold ends the moment a decorated snapshot arrives, or when
        // `HIGHLIGHT_WAIT` elapses (a slow parse / missing grammar must not stall the
        // screen). An overlay open (picker/palette) paints regardless - it is not the
        // buffer body.
        let hold_for_highlight = awaiting_highlight.is_some_and(|since| {
            overlays.is_empty()
                && latest.as_ref().is_some_and(|s| s.decorations.is_empty())
                && since.elapsed() < HIGHLIGHT_WAIT
        });

        if let Some(snap) = &latest
            && needs_redraw
            && !hold_for_highlight
        {
            // A switch swaps the viewport: park the outgoing buffer's scroll and
            // restore the incoming one's, so coming back lands where you left rather
            // than snapping to the top. A buffer never painted before starts at the
            // default, which is the top.
            if painted != Some(snap.buffer_id) {
                if let Some(previous) = painted {
                    viewports.insert(previous, viewport);
                }
                viewport = viewports.get(&snap.buffer_id).copied().unwrap_or_default();
                painted = Some(snap.buffer_id);
            }
            viewport = draw(
                terminal,
                snap,
                PaintInputs {
                    viewport,
                    theme: config.theme,
                    follow,
                    selected,
                    tab_width: config.tab_width,
                    search: &search,
                },
                &overlays,
                &toasts,
            )?;
            needs_redraw = false;
            awaiting_highlight = None;
            // Default back to caret-follow; only a wheel scroll opts out, and only
            // for the single frame it triggered.
            follow = true;
        }

        // Wait for input, but no longer than POLL so a snapshot arriving without a
        // keystroke still gets painted on the next tick.
        if event::poll(POLL)? {
            let input = event::read()?;
            // Any input ends a pending highlight-hold: a keystroke (or click) must be
            // reflected at once, never delayed waiting on syntax colors.
            awaiting_highlight = None;
            match input {
                Event::Key(key) => {
                    // Ignore key *releases* (the Kitty protocol reports them, SPEC
                    // §9): acting on press and release would double-fire, the same
                    // rule the keymap applies. Skipping early also shields overlays.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // Overlays get first refusal (SPEC §7.5): a prompt consumes its
                    // keys so they stay frontend-local; only a *committed* choice
                    // (e.g. a submitted path) comes back as a `Command` to dispatch.
                    if !overlays.is_empty() {
                        let (result, commands) = overlays.handle_key(key);
                        needs_redraw = true;
                        for command in commands {
                            let mut ui = Frontend {
                                overlays: &mut overlays,
                                config: &mut config,
                                toasts: &mut toasts,
                                snapshot: latest.as_ref(),
                                search: &mut search,
                            };
                            if !dispatch_command(command, handle, &mut ui) {
                                return Ok(());
                            }
                        }
                        if result == EventResult::Consumed {
                            continue;
                        }
                    }
                    // Otherwise the keymap resolves the key to a frontend command
                    // (SPEC §7.5): a UI trigger (Ctrl+O) opens an overlay, any other
                    // key forwards its core intent. Routed through the keymap, not an
                    // inline branch, so the binding is data (user-configurable at M5).
                    // Page size is folded into page motions here (only the frontend
                    // knows it, SPEC §5).
                    if let Some(command) =
                        keymap::command_for_key(&config.keymap, key, viewport.page())
                    {
                        // If the binding fired *over* an open overlay (a picker
                        // deferred its shortcut, SPEC §7.5), the shortcut dismisses the
                        // overlay and takes precedence. Config-friendly: the binding
                        // comes from the keymap, the single source the palette shows.
                        if !overlays.is_empty() {
                            overlays.dismiss();
                            needs_redraw = true;
                        }
                        // A UI overlay (any non-`Editor` command) opens locally, so
                        // repaint now; a core intent repaints when its snapshot
                        // returns, so it need not force one.
                        if !matches!(&command, Command::Editor(_)) {
                            needs_redraw = true;
                        }
                        let mut ui = Frontend {
                            overlays: &mut overlays,
                            config: &mut config,
                            toasts: &mut toasts,
                            snapshot: latest.as_ref(),
                            search: &mut search,
                        };
                        if !dispatch_command(command, handle, &mut ui) {
                            return Ok(());
                        }
                    }
                }
                // An overlay owns the screen as well as the keyboard (SPEC §7.5), so
                // it gets first refusal on the pointer too. What it does not consume
                // falls through to the editor below, exactly as a key does - which is
                // how a click *outside* a dismissable overlay can both close it and
                // land where the user aimed.
                // Bare pointer motion, which mode 1003 reports for every cell the
                // pointer crosses. Nothing hovers today, and acting on it would mean
                // a repaint per cell of mouse travel, so it is dropped before it can
                // cost anything. The moment something does hover, this is where it
                // starts.
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {}
                Event::Mouse(mouse) if !overlays.is_empty() => {
                    let screen = terminal.size()?;
                    let (result, commands) =
                        overlays.handle_mouse(mouse, Rect::new(0, 0, screen.width, screen.height));
                    for command in commands {
                        let mut ui = Frontend {
                            overlays: &mut overlays,
                            config: &mut config,
                            toasts: &mut toasts,
                            snapshot: latest.as_ref(),
                            search: &mut search,
                        };
                        if !dispatch_command(command, handle, &mut ui) {
                            return Ok(());
                        }
                    }
                    needs_redraw = true;
                    if result == EventResult::Consumed {
                        continue;
                    }
                }
                // A toast sits over the editor, so a click on one is a click on it
                // rather than on the text underneath - dismissing it early instead of
                // waiting out the fade. Checked before the editor's own handling and
                // only for a press, so a drag that happens to pass under a toast
                // keeps sweeping out its selection.
                Event::Mouse(mouse)
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && toasts.dismiss_at(
                            Rect::new(0, 0, terminal.size()?.width, terminal.size()?.height),
                            mouse.column,
                            mouse.row,
                        ) =>
                {
                    needs_redraw = true;
                }
                Event::Mouse(mouse) => match mouse.kind {
                    // Left press or drag places/extends the caret at the pointer.
                    // A press is a plain click unless Shift is held (extend from the
                    // current anchor); a drag always extends, so a press-then-drag
                    // sweeps out a selection.
                    MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left) => {
                        let Some(snap) = &latest else { continue };
                        let is_press = matches!(mouse.kind, MouseEventKind::Down(_));
                        // A press on the bufferline selects that tab's buffer rather
                        // than placing a caret: row 0 is chrome, not text. Resolved
                        // against the same fitted strip that was painted, so the tab
                        // under the pointer is the one that gets picked. A drag is
                        // ignored here - dragging across tabs is not a gesture.
                        if is_press && mouse.row == 0 {
                            let count_width =
                                layout::line_count_label(layout::display_line_count(&snap.text))
                                    .width();
                            let tabs = layout::head_bar_tabs(
                                &snap.buffers,
                                snap.buffer_id,
                                count_width,
                                terminal.size()?.width as usize,
                            );
                            // A click on an overflow marker, the padding, or the line
                            // count selects nothing and is simply swallowed.
                            if let Some(id) = layout::tab_at_column(&tabs, mouse.column as usize)
                                && id != snap.buffer_id
                                && handle
                                    .actions
                                    .send_blocking(Action::SwitchBuffer { id })
                                    .is_err()
                            {
                                return Ok(());
                            }
                            continue;
                        }
                        // The status bar is a readout *and* a control: its encoding
                        // and line-ending words open a picker for what they show
                        // (SPEC §7.5). Checked before the body, since the bar is not
                        // text - and only on the bottom row, so a drag that ends
                        // there still belongs to the selection it was sweeping.
                        let bottom = terminal.size()?.height.saturating_sub(1);
                        if is_press && mouse.row == bottom {
                            if let Some(target) = status_target_at(
                                snap,
                                selected,
                                terminal.size()?.width as usize,
                                mouse.column as usize,
                            ) {
                                let command = match target {
                                    layout::StatusTarget::Encoding => Command::OpenEncodingPicker,
                                    layout::StatusTarget::LineEnding => {
                                        Command::OpenLineEndingPicker
                                    }
                                };
                                let mut ui = Frontend {
                                    overlays: &mut overlays,
                                    config: &mut config,
                                    toasts: &mut toasts,
                                    snapshot: latest.as_ref(),
                                    search: &mut search,
                                };
                                if !dispatch_command(command, handle, &mut ui) {
                                    return Ok(());
                                }
                                needs_redraw = true;
                            }
                            continue;
                        }
                        // Only a plain press advances the click run (SPEC §2.2): a
                        // drag is one continuous gesture, and a modified click means
                        // something else entirely, so both end the run rather than
                        // extending it. Counted here because it is the one part of
                        // the decision that needs a clock.
                        let count = if is_press
                            && !mouse
                                .modifiers
                                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                        {
                            clicks.press(mouse.column, mouse.row, Instant::now())
                        } else {
                            clicks.reset();
                            1
                        };
                        let action = press_action(snap, viewport, config.tab_width, mouse, count);
                        if handle.actions.send_blocking(action).is_err() {
                            return Ok(());
                        }
                    }
                    // Wheel scrolls the viewport without moving the caret (follow
                    // off for this frame); clamping to content happens in `paint`.
                    MouseEventKind::ScrollDown => {
                        viewport.scroll = viewport.scroll.saturating_add(SCROLL_STEP);
                        follow = false;
                        needs_redraw = true;
                    }
                    MouseEventKind::ScrollUp => {
                        viewport.scroll = viewport.scroll.saturating_sub(SCROLL_STEP);
                        follow = false;
                        needs_redraw = true;
                    }
                    _ => {}
                },
                // While an overlay is open, swallow OS pastes too rather than
                // splatting the text into the buffer underneath (SPEC §7.5 modal).
                // Pasting *into* the prompt is an M7 refinement.
                Event::Paste(_) if !overlays.is_empty() => {}
                // An OS paste (bracketed paste): insert the whole payload as one
                // action (SPEC §6), splatting the external text at every cursor. This
                // is distinct from the editor's own `paste` command, which pulls the
                // core's structured register; the terminal only ever hands us a flat
                // string, so `Insert` is the right intent here.
                Event::Paste(text) => {
                    if handle.actions.send_blocking(Action::Insert(text)).is_err() {
                        return Ok(());
                    }
                }
                // Repaint against the new terminal size.
                Event::Resize(_, _) => needs_redraw = true,
                _ => {}
            }
        }
    }
}

/// The frontend-local state a dispatched command may touch. Bundled rather than
/// passed as four more parameters, the same consolidation as [`PaintInputs`]: a
/// theme change writes the live config *and* has to repaint the overlay stack and
/// the toast surface that cached its styles.
struct Frontend<'a> {
    overlays: &'a mut Compositor,
    config: &'a mut config::Config,
    toasts: &'a mut Toasts,
    /// The newest snapshot, or `None` before the first one arrives. Held whole rather
    /// than as pre-extracted fields: the commands need three different projections of
    /// it, and every new one would otherwise mean another field threaded through
    /// every construction site.
    snapshot: Option<&'a ViewSnapshot>,
    /// What the frontend remembers about searching (SPEC §11): the live query, the
    /// previewed match, and the pattern a find-next key repeats. Frontend-side
    /// because all three are frontend facts - the preview never crosses the seam, and
    /// a core that also remembered a pattern could disagree with what is on screen.
    search: &'a mut buffersearch::SearchState,
}

impl Frontend<'_> {
    /// The active buffer's file path, so the save-as prompt can pre-fill it.
    fn path(&self) -> Option<&std::path::Path> {
        self.snapshot?.path.as_deref()
    }

    /// Every open buffer in bufferline order.
    fn buffers(&self) -> &[BufferInfo] {
        self.snapshot.map_or(&[], |s| &s.buffers)
    }

    /// The buffer on screen.
    fn active(&self) -> Option<BufferId> {
        Some(self.snapshot?.buffer_id)
    }

    /// The primary caret's byte offset - where a search starts from.
    fn caret(&self) -> usize {
        self.snapshot
            .and_then(|s| s.selections.get(s.primary))
            .map_or(0, |s| s.head)
    }

    /// The buffer `offset` positions from the active one, or `None` when there is
    /// nothing to switch to (no snapshot yet, or a single buffer). This is what lets
    /// "next buffer" be resolved here rather than in the core: the frontend already
    /// holds the order, so it names the neighbor and only the committed
    /// `SwitchBuffer { id }` crosses the seam (SPEC §7.5). The wrapping lives in
    /// [`command::neighbor_buffer`], which is pure and unit-tested.
    fn neighbor(&self, offset: isize) -> Option<BufferId> {
        command::neighbor_buffer(self.buffers(), self.active()?, offset)
    }
}

/// Dispatch one resolved frontend command (SPEC §7.5), from either a bound key or a
/// compositor layer committing a choice - one path for both. A core intent is
/// forwarded to the actor; a UI command opens an overlay or restyles the frontend.
/// Returns `false` when the app should exit (a quit, or the core's action channel
/// closed).
fn dispatch_command(command: Command, handle: &vortex_core::CoreHandle, ui: &mut Frontend) -> bool {
    match command {
        Command::Editor(action) => {
            // Escape means "back to one cursor" *and* "done searching" (SPEC §11).
            // Without this the highlights from a committed search would stay up with
            // no way to dismiss them short of searching for something else - the
            // prompt's own Escape is gone by then, having closed the prompt.
            if action == Action::CollapseSelections {
                ui.search.clear();
            }
            let quit = action == Action::Quit;
            if handle.actions.send_blocking(action).is_err() || quit {
                return false;
            }
        }
        // The palette shows each command's shortcut, so it needs the keymap too.
        Command::OpenPalette => ui
            .overlays
            .push(palette::open(&ui.config.theme, &ui.config.keymap)),
        Command::OpenFilePicker => {
            // Walk the working directory. If it cannot be read, fall back to ".".
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            ui.overlays.push(filepicker::open(&ui.config.theme, &root));
        }
        Command::OpenSearchPicker => {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            ui.overlays
                .push(globalsearch::open(&ui.config.theme, &root));
        }
        // A hit is an arrival, not just an open: the two actions go down the same
        // channel in order, so the jump resolves against the buffer the open just
        // produced (SPEC §7.5 - the frontend owns "where", the core owns the text).
        // The jump names the file it was computed against, so an open that fails -
        // the hit is only as fresh as the walk that found it - drops the jump
        // instead of moving the caret in whatever buffer is focused.
        Command::OpenAt { path, position } => {
            if handle
                .actions
                .send_blocking(Action::Open(path.clone()))
                .is_err()
            {
                return false;
            }
            if handle
                .actions
                .send_blocking(Action::PlaceCursorAt {
                    position,
                    in_file: Some(path),
                })
                .is_err()
            {
                return false;
            }
        }
        Command::OpenThemePicker => ui
            .overlays
            .push(themepicker::open(&ui.config.theme, &ui.config.theme_name)),
        // No I/O and no round trip: the rows come from the snapshot's buffer list.
        // Nothing to pick from before the first snapshot, so it simply does not open.
        Command::OpenBufferPicker => {
            if let Some(active) = ui.active() {
                let buffers = ui.buffers();
                ui.overlays
                    .push(bufferpicker::open(&ui.config.theme, buffers, active));
            }
        }
        // Both format pickers open on what the buffer currently is, which they read
        // from the snapshot - the same value the status bar prints, so the readout
        // and the chooser can never disagree. No snapshot yet means nothing to
        // change, so they do not open.
        Command::OpenEncodingPicker => {
            if let Some(format) = ui.snapshot.map(|s| s.format) {
                ui.overlays.push(formatpicker::encoding(
                    &ui.config.theme,
                    format.encoding_name(),
                ));
            }
        }
        Command::OpenLineEndingPicker => {
            if let Some(format) = ui.snapshot.map(|s| s.format) {
                ui.overlays
                    .push(formatpicker::line_ending(&ui.config.theme, format.eol));
            }
        }
        // The save-as prompt pre-fills the current path (if any) so a save-as is a
        // quick edit of the existing name; its committed path returns as SaveAs.
        Command::OpenSavePrompt => ui
            .overlays
            .push(prompt::save_as(&ui.config.theme, ui.path())),
        // The buffer commands resolve against the list the snapshot carries and then
        // share one send below, since each is "work out which buffer, then say so".
        // Nothing to say (one buffer open, or no snapshot yet) is a no-op rather than
        // a round trip.
        Command::NextBuffer | Command::PrevBuffer | Command::CloseBuffer => {
            let action = match command {
                Command::NextBuffer => ui.neighbor(1).map(|id| Action::SwitchBuffer { id }),
                Command::PrevBuffer => ui.neighbor(-1).map(|id| Action::SwitchBuffer { id }),
                // Unforced: the core refuses if there is unsaved work, and its
                // `CloseRejected` is what raises the confirmation (SPEC §8).
                _ => ui
                    .active()
                    .map(|id| Action::CloseBuffer { id, force: false }),
            };
            if let Some(action) = action
                && handle.actions.send_blocking(action).is_err()
            {
                return false;
            }
        }
        // In-buffer search (SPEC §11). The prompt is seeded with the last pattern -
        // reopening a search offers it back rather than making it be retyped - and
        // the search begins from wherever the caret is, which is what the preview
        // measures "the next match" from as the query is refined.
        Command::OpenFindPrompt { replacing } => {
            let seed = ui
                .search
                .query()
                .map(|q| q.pattern.clone())
                .unwrap_or_default();
            ui.search.begin(ui.caret());
            ui.overlays.push(Box::new(buffersearch::Find::new(
                &ui.config.theme,
                seed,
                replacing,
            )));
        }
        // Per keystroke, and it goes no further than here: the highlights and the
        // previewed match are painted from the frontend's own copy of the text
        // (SPEC §5), so a search in progress has changed nothing that needs undoing.
        Command::PreviewSearch {
            pattern,
            replacement,
        } => ui.search.refresh(
            buffersearch::Query::new(pattern, replacement),
            ui.snapshot.map(|s| &s.text),
        ),
        Command::ClearSearch => ui.search.clear(),
        // The pattern lives here, so these fill it in and send a complete action.
        // Nothing searched for yet means nothing to repeat - a no-op, not a round
        // trip, the same shape as a buffer switch with one buffer open.
        Command::FindNext | Command::FindPrevious | Command::SelectAllMatches => {
            ui.search.commit();
            let backward = command == Command::FindPrevious;
            let action = match command {
                Command::SelectAllMatches => ui.search.query().map(|q| Action::SelectAllMatches {
                    pattern: q.pattern.clone(),
                }),
                _ => ui.search.repeat(backward),
            };
            if let Some(action) = action
                && handle.actions.send_blocking(action).is_err()
            {
                return false;
            }
        }
        // The replace prompt committed both halves; the walk asks about each match in
        // turn from here. It starts by *finding* rather than replacing, so the first
        // question is always about a match the user can see.
        Command::StartReplace => {
            let Some((pattern, replacement)) = ui
                .search
                .query()
                .map(|q| (q.pattern.clone(), q.replacement.clone()))
                .filter(|(pattern, _)| !pattern.is_empty())
            else {
                return true;
            };
            // Whether there is anything to walk is answerable here, against the same
            // text the preview used - so a pattern that matches nothing gets the
            // core's one "no matches" and no walk. Opening it anyway would ask the
            // user to agree to replacing something that does not exist, and every
            // answer would fire another notification for the match it failed to find.
            let any_match = ui
                .snapshot
                .zip(ui.search.query())
                .is_some_and(|(snap, query)| query.next_from(&snap.text, ui.caret()).is_some());
            ui.search.commit();
            if handle
                .actions
                .send_blocking(Action::SelectNextMatch {
                    pattern: pattern.clone(),
                    backward: false,
                })
                .is_err()
            {
                return false;
            }
            if any_match {
                ui.overlays.push(Box::new(buffersearch::QueryReplace::new(
                    &ui.config.theme,
                    pattern,
                    replacement,
                )));
            }
        }
        // Chrome is frontend-owned, so a theme change never crosses the seam: swap
        // the live config and hand the new styles to the surfaces that cached them.
        // A theme file that will not load must say so (SPEC §8: never silent) - and
        // the theme in use is left alone rather than half-applied.
        Command::SetTheme(name) => match theme::load_named(&name) {
            Ok(theme) => {
                ui.config.theme = theme;
                ui.config.theme_name = name;
                ui.toasts
                    .restyle(ui.config.theme.toast_info, ui.config.theme.toast_error);
                ui.overlays.restyle(&ui.config.theme);
            }
            Err(message) => ui.toasts.push(message, toast::Level::Error, Instant::now()),
        },
    }
    true
}

/// Resolve an absolute pointer cell to a buffer byte offset, using the last painted
/// viewport (gutter width, scroll on both axes) so the lookup needs no core
/// round-trip (SPEC §5). The head bar occupies screen row 0, so the body row is
/// `row - 1`, clamped into the painted text rows: a click on the head bar maps to
/// the top visible line and a drag below the body to the last one. Column and
/// end-of-line clamping are handled by [`layout::offset_at_cell`].
fn pointer_offset(
    snapshot: &ViewSnapshot,
    viewport: ViewState,
    tab_width: usize,
    column: u16,
    row: u16,
) -> usize {
    let gutter_width = layout::gutter_width(layout::display_line_count(&snapshot.text));
    let last_body_row = viewport.page_height.saturating_sub(1);
    let body_row = (row.saturating_sub(1) as usize).min(last_body_row);
    layout::offset_at_cell(
        &snapshot.text,
        viewport.scroll,
        viewport.h_scroll,
        gutter_width,
        tab_width,
        body_row,
        column as usize,
    )
}

/// Paint one frame from `snapshot`, wrapped in synchronized output (anti-tearing,
/// SPEC §7). Returns the (possibly adjusted) viewport so the primary cursor stays
/// visible on both axes. The frame composition itself lives in [`paint`] so it can
/// be rendered against a `TestBackend` and asserted cell-by-cell (SPEC §13).
fn draw(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    snapshot: &ViewSnapshot,
    inputs: PaintInputs,
    overlays: &Compositor,
    toasts: &Toasts,
) -> io::Result<ViewState> {
    let mut new_viewport = inputs.viewport;
    let mut out = io::stdout();
    queue!(out, BeginSynchronizedUpdate)?;
    terminal.draw(|frame| {
        new_viewport = paint(frame, snapshot, inputs);
        let area = frame.area();
        // Toasts paint over the base editor but consume no input (SPEC §7.5), then
        // overlays paint over everything. The focused overlay owns the caret, so its
        // cursor - set last - wins over the editor caret `paint` placed. (A menu-style
        // overlay wanting no caret at all is an M7 concern; today's only overlay, the
        // prompt, always provides one.)
        toasts.render(area, frame.buffer_mut());
        overlays.render(area, frame.buffer_mut());
        if let Some(pos) = overlays.cursor(area) {
            frame.set_cursor_position(pos);
        }
    })?;
    execute!(out, EndSynchronizedUpdate)?;
    Ok(new_viewport)
}

/// Everything one frame needs beyond the snapshot itself: the carried view
/// state, theme, caret-follow flag, and the selection count precomputed by the
/// event loop. Bundled as one `Copy` value (the same consolidation as
/// [`ViewState`]/[`Body`]) so `draw`/`paint` stay within the argument budget as
/// per-frame inputs grow.
#[derive(Clone, Copy)]
struct PaintInputs<'a> {
    /// The view state carried from the previous frame.
    viewport: ViewState,
    /// The active theme.
    theme: config::Theme,
    /// Whether this frame pulls the viewport to keep the caret visible (off only
    /// for the single frame after a wheel scroll).
    follow: bool,
    /// Grapheme count of the active selection, computed once per snapshot by the
    /// event loop (O(selected bytes) - too costly to re-derive every repaint).
    selected: usize,
    /// Display width of a tab stop (SPEC §4, §10.5). Carried per frame rather than
    /// read from a constant so a config change takes effect without a restart.
    tab_width: usize,
    /// The live search (SPEC §11), borrowed rather than copied so the highlights can
    /// never be a frame behind what the prompt is showing.
    ///
    /// Its matches are resolved **per frame over the visible lines only**, not
    /// cached: the buffer beneath a search can change - an undo, an external reload -
    /// and a cached set of byte ranges would then paint over text that has moved. The
    /// scan is viewport-bounded, which is what makes recomputing it every frame the
    /// cheap option rather than the careful one (SPEC §10.4).
    search: &'a buffersearch::SearchState,
}

/// Compose the whole frame: head bar, gutter + text, status bar, and the cursor.
/// Backend-generic (takes a `&mut Frame`) so a `TestBackend` render can assert on
/// the painted cells (SPEC §13). Returns the scroll offset it settled on so the
/// caller can carry it forward. All measurement is delegated to the tested
/// [`layout`] helpers; this function only positions widgets.
fn paint(frame: &mut Frame, snapshot: &ViewSnapshot, inputs: PaintInputs) -> ViewState {
    let PaintInputs {
        viewport,
        theme,
        follow,
        selected,
        tab_width,
        search,
    } = inputs;
    let area = frame.area();
    // Head bar (1 row), text body (rest), status bar (1 row). `Min(0)` lets the
    // body shrink to nothing on a tiny terminal without the split failing.
    let [head_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    // Primary cursor position in line/grapheme-column space (SPEC §2.2): follow
    // the primary selection, not a positional guess.
    let head = snapshot
        .selections
        .get(snapshot.primary)
        .map(|s| s.head)
        .unwrap_or(0);
    let (cursor_line, cursor_byte_col, line_text) = layout::cursor_line_col(&snapshot.text, head);
    let cursor_display_col = layout::display_column(&line_text, cursor_byte_col, tab_width);

    // While a find prompt is open the viewport follows the *previewed match* instead
    // of the caret (SPEC §11): that is what makes typing a query show you where you
    // would land, and it is a pure frontend move - the caret has not gone anywhere,
    // so cancelling the search leaves nothing to put back. Only the vertical axis
    // follows it; horizontal scrolling to a match on a long line would yank the view
    // sideways on every keystroke.
    let preview = search.preview();
    let follow_line = match &preview {
        Some(range) => snapshot.text.line_of_byte(range.start),
        None => cursor_line,
    };

    // Gutter width is fixed for the frame; the text column budget is what's left of
    // the body after it. Both scroll axes follow the cursor minimally (SPEC §5):
    // vertical by line, horizontal by display column within the text area.
    let text_height = body_area.height as usize;
    let display_lines = layout::display_line_count(&snapshot.text);
    let gutter_width = layout::gutter_width(display_lines);
    let text_width = (body_area.width as usize).saturating_sub(gutter_width);
    // `scroll_to_show` only scrolls *toward* the cursor, never capping the offset at
    // the content extent. A stale offset carried across a frame where the buffer (or
    // line) shrank would then paint blank rows/columns and hide content that fits, so
    // clamp each axis to its max useful offset. The `+ 1` on the horizontal extent
    // leaves a cell for the caret sitting just past the line's last glyph.
    let max_scroll = display_lines.saturating_sub(text_height);
    let line_width = layout::display_column(&line_text, line_text.len(), tab_width);
    let max_h_scroll = (line_width + 1).saturating_sub(text_width);
    // When following the caret (keys, clicks, edits) each axis scrolls the minimum
    // to keep it visible; a wheel scroll turns follow off and paints the viewport's
    // own offset instead, so the view can move away from the caret. Both are clamped
    // to the content extent so a stale offset never paints blank rows/columns.
    // A live preview overrides the wheel's "stay put": the user is asking to be shown
    // the match, which is the one case where the view must move without the caret.
    let scroll = if follow || preview.is_some() {
        layout::scroll_to_show(follow_line, viewport.scroll, text_height)
    } else {
        viewport.scroll
    }
    .min(max_scroll);
    let h_scroll = if follow {
        layout::scroll_to_show(cursor_display_col, viewport.h_scroll, text_width)
    } else {
        viewport.h_scroll
    }
    .min(max_h_scroll);

    paint_head_bar(frame, head_area, snapshot, &theme);
    paint_body(
        frame,
        body_area,
        snapshot,
        Body {
            scroll,
            h_scroll,
            gutter_width,
            text_width,
            cursor_line,
            theme,
            tab_width,
            // Only the lines about to be painted are searched, which is the whole
            // reason `matches_in` takes a line range (SPEC §10.4).
            matches: search
                .query()
                .map(|q| q.matches_in(&snapshot.text, scroll..scroll + text_height))
                .unwrap_or_default(),
            current: preview,
        },
    );
    paint_status_bar(
        frame,
        status_area,
        snapshot,
        StatusBar {
            cursor_line,
            line_text: &line_text,
            cursor_byte_col,
            selected,
            style: theme.status_bar,
        },
    );

    // Place the terminal cursor at the primary caret, offset by the gutter and the
    // head row. Only when the caret is within the visible window on both axes: a
    // wheel scroll can push it out of view, and a cursor pinned to a screen edge
    // then would be wrong - ratatui hides the cursor when `paint` sets no position.
    let cursor_visible = text_height > 0
        && (scroll..scroll + text_height).contains(&cursor_line)
        && (h_scroll..h_scroll + text_width).contains(&cursor_display_col);
    if cursor_visible {
        let row = body_area.y + (cursor_line - scroll) as u16;
        let col = body_area.x + (gutter_width + cursor_display_col - h_scroll) as u16;
        frame.set_cursor_position(Position::new(col, row));
    }

    ViewState {
        scroll,
        h_scroll,
        page_height: text_height,
    }
}

/// Paint the top head bar (buffer name left, line count right) as one filled row.
/// The name is the bound file's name plus a modified marker (SPEC §8, §10), read
/// straight from the snapshot so painting needs no core round-trip (SPEC §5).
fn paint_head_bar(frame: &mut Frame, area: Rect, snapshot: &ViewSnapshot, theme: &config::Theme) {
    let width = area.width as usize;
    // The line count keeps the right end; the tab strip gets what is left. With one
    // buffer open that reads exactly as the pre-bufferline head bar did.
    let count = layout::line_count_label(layout::display_line_count(&snapshot.text));
    let count_width = count.width();
    let tabs = layout::head_bar_tabs(&snapshot.buffers, snapshot.buffer_id, count_width, width);

    let mut spans: Vec<Span> = Vec::with_capacity(tabs.len() + 2);
    let mut used = 0;
    for tab in tabs {
        // Each segment already knows its width; re-measuring the label here would be
        // the third walk over the same text in one frame.
        used += tab.cells;
        let style = match tab.kind {
            layout::Segment::Tab { active: true, .. } => theme.head_bar_active,
            layout::Segment::Tab { active: false, .. } => theme.head_bar_inactive,
            layout::Segment::Chrome => theme.head_bar_separator,
        };
        spans.push(Span::styled(tab.label, style));
    }
    // Pad between the strip and the count so the count sits flush right. The pad and
    // the count are governed by the same "does it fit" question.
    if let Some(gap) = width.checked_sub(used + count_width) {
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap), theme.head_bar));
        }
        spans.push(Span::styled(count, theme.head_bar));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.head_bar),
        area,
    );
}

/// The resolved geometry for painting the text body, computed once in [`paint`]
/// and handed to [`paint_body`] as one value instead of five positional args
/// (the same consolidation as [`ViewState`], and it lets `text_width` be computed
/// in one place rather than recomputed here).
struct Body {
    /// Top visible line (post-scroll).
    scroll: usize,
    /// Leftmost visible display column (post-scroll).
    h_scroll: usize,
    /// Gutter width in cells (fixed; never scrolls horizontally).
    gutter_width: usize,
    /// Display-column budget for text, right of the gutter.
    text_width: usize,
    /// The live search's matches within the visible lines, in buffer bytes, ascending
    /// (SPEC §11). Empty when nothing is being searched for, which is the common case
    /// and costs the paint nothing.
    matches: Vec<std::ops::Range<usize>>,
    /// Which match the search is *on* - painted in the accent so "which one is next"
    /// is answerable on a screen full of hits.
    current: Option<std::ops::Range<usize>>,
    /// The cursor's line, so its gutter number can be emphasized.
    cursor_line: usize,
    /// The active theme, read straight through for the chrome styles this paints
    /// (gutter, selection, current line, secondary caret). `Theme` is `Copy`, so
    /// holding it beats copying each style field out by hand.
    theme: config::Theme,
    /// Display width of a tab stop (SPEC §4, §10.5).
    tab_width: usize,
}

/// Paint the text body with a line-number gutter. Each visible row is a gutter
/// span (dim, or bold for the cursor's line) followed by the tab-expanded line
/// rendered to styled spans over the horizontal window `[h_scroll, h_scroll +
/// text_width)`. The gutter is fixed (never scrolls horizontally); only the text
/// slides under it. Overlays tint the text: the cursor's row gets a full-width
/// [`Body::current_line`] wash (via the row's base style), every selection paints
/// [`Body::selection`] over the columns it covers, and every *non-primary* caret
/// gets a one-cell [`Body::secondary_cursor`] block so a multi-cursor set is visible
/// (SPEC §2.2 - the primary caret renders as the terminal's own cursor, so its
/// zero-width selection shows nothing here).
fn paint_body(frame: &mut Frame, area: Rect, snapshot: &ViewSnapshot, body: Body) {
    let text = &snapshot.text;
    let height = area.height as usize;
    let lines = layout::visible_lines(text, body.scroll, height, body.tab_width);

    // Each secondary caret's line is invariant across the frame: resolve it once
    // here (O(selections) rope lookups) instead of per visible row, which would
    // be O(rows x selections) once M3 multi-cursor grows both factors.
    let secondary_carets: Vec<(usize, usize)> = snapshot
        .selections
        .iter()
        .enumerate()
        .filter(|&(i, s)| i != snapshot.primary && s.is_cursor())
        .map(|(_, s)| (text.line_of_byte(s.head), s.head))
        .collect();

    let rows: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let line_index = body.scroll + row;
            let is_current = line_index == body.cursor_line;
            // The cursor row's tint fills the whole width, so it is the text's base
            // style and is patched onto the gutter number too for a continuous row.
            let (base, gutter_style) = if is_current {
                (
                    body.theme.current_line,
                    body.theme.gutter_current.patch(body.theme.current_line),
                )
            } else {
                (Style::default(), body.theme.gutter)
            };
            // Selection overlays for this line, in display columns. The raw line
            // (tabs intact) and its byte span drive the byte->column mapping; the
            // rendered text is the tab-expanded `content`.
            let line_start = text.byte_of_line(line_index).unwrap_or(0);
            let line_end_excl = text
                .byte_of_line(line_index + 1)
                .unwrap_or_else(|| text.byte_len());
            // `visible_lines` already fetched this line; reuse its raw form for the
            // byte->column mapping rather than a second rope traversal per row.
            let raw = &line.raw;
            // Selection washes first, so syntax highlights paint *over* them: a
            // selection sets a background, and the highlight that follows patches
            // only the foreground, so selected code keeps its syntax colors on the
            // selection's ground rather than being flattened to the selection's own
            // foreground (SPEC §5, later overlays win in `render_line`).
            let mut overlays: Vec<(std::ops::Range<usize>, Style)> = snapshot
                .selections
                .iter()
                .filter_map(|s| {
                    layout::selection_columns(
                        raw,
                        line_start,
                        line_end_excl,
                        body.tab_width,
                        s.start(),
                        s.end(),
                    )
                    .map(|range| (range, body.theme.selection))
                })
                .collect();
            // Search matches (SPEC §11), pushed with the selection washes and so
            // *under* the syntax highlights: a match sets a ground, and the
            // highlight that follows patches only the foreground, so matched code
            // keeps its syntax colors exactly as selected code does. The one the
            // search is on takes the accent instead of the wash.
            for span in body.matches.iter().filter(|m| {
                // The set spans the whole viewport; this row wants its own slice.
                m.start < line_end_excl && m.end > line_start
            }) {
                if let Some(range) = layout::selection_columns(
                    raw,
                    line_start,
                    line_end_excl,
                    body.tab_width,
                    span.start,
                    span.end,
                ) {
                    let style = if body.current.as_ref() == Some(span) {
                        body.theme.search_current
                    } else {
                        body.theme.search_match
                    };
                    overlays.push((range, style));
                }
            }
            // Syntax highlights (M4): each span clipped to this line's byte range,
            // mapped to display columns, painted as a foreground color over the
            // selection ground and under the diagnostic underline and carets.
            if !snapshot.decorations.is_empty() {
                // Highlights arrive sorted and non-overlapping, so one walker
                // resolves every span on this line in a single left-to-right pass
                // (O(line + spans)) instead of rescanning from byte 0 per span.
                let mut walker = layout::ColumnWalker::new(raw, body.tab_width);
                for (span, kind) in snapshot
                    .decorations
                    .highlights_in(line_start..line_end_excl)
                {
                    if let Some(range) = layout::span_columns(
                        &mut walker,
                        raw.len(),
                        line_start,
                        line_end_excl,
                        span.start,
                        span.end,
                    ) {
                        overlays.push((range, body.theme.highlight(kind)));
                    }
                }
            }
            // Diagnostic underlines (SPEC §5): the decoration channel resolved for
            // just this line's byte span, each clipped to its columns and painted
            // as an underlined foreground. Pushed before the caret blocks so a
            // secondary cursor still shows on top of a squiggle sharing its cell.
            if !snapshot.decorations.is_empty() {
                for (span, severity) in snapshot
                    .decorations
                    .underlines_in(line_start..line_end_excl)
                {
                    if let Some(range) = layout::selection_columns(
                        raw,
                        line_start,
                        line_end_excl,
                        body.tab_width,
                        span.start,
                        span.end,
                    ) {
                        let style = body
                            .theme
                            .diagnostic(severity)
                            .add_modifier(Modifier::UNDERLINED);
                        overlays.push((range, style));
                    }
                }
            }
            // Mark every secondary (non-primary) caret with a one-cell block so a
            // multi-cursor set is visible: the terminal has a single real cursor,
            // which the primary uses (SPEC §2.2). Pushed after the selection washes
            // so a caret shows on top of any highlight sharing its cell.
            for &(line, head) in &secondary_carets {
                if line == line_index {
                    let col = layout::display_column(raw, head - line_start, body.tab_width);
                    overlays.push((col..col + 1, body.theme.secondary_cursor));
                }
            }

            // A diagnostic on this line recolors its gutter number with the
            // severity's color (SPEC §5): a signal in the margin without widening
            // the gutter, so the line-number layout math is untouched. The most
            // severe mark on the line wins (`gutter_mark` picks it).
            let gutter_style = match snapshot.decorations.gutter_mark(text, line_index) {
                Some(vortex_core::GutterKind::Diagnostic(severity)) => gutter_style
                    .patch(body.theme.diagnostic(severity))
                    .add_modifier(Modifier::BOLD),
                // `GutterKind` is non-exhaustive (git signs join in M8); an
                // unknown kind leaves the gutter as-is rather than failing.
                _ => gutter_style,
            };
            let mut spans = vec![Span::styled(
                layout::gutter_label(line_index, body.gutter_width),
                gutter_style,
            )];
            spans.extend(layout::render_line(
                line.display(),
                body.h_scroll,
                body.text_width,
                base,
                &overlays,
            ));
            Line::from(spans)
        })
        .collect();

    // The theme's ground is the base style for the whole body area, so it covers
    // the rows past the end of the buffer too and a light theme is legible in a dark
    // terminal. Per-row styles (current line, selection) patch on top of it.
    frame.render_widget(Paragraph::new(rows).style(body.theme.text), area);
}

/// Paint the bottom status bar: cursor position (left) and buffer metrics (right).
/// File open/save results surface as toasts now (SPEC §7.5), so the position is
/// always shown here.
/// The per-frame inputs [`paint_status_bar`] needs beyond the frame/area/snapshot:
/// the cursor readout and the bar style (from the active theme). Bundled as one
/// value so the painter stays within the argument budget, the same consolidation as
/// [`Body`].
struct StatusBar<'a> {
    /// 0-based cursor line (displayed 1-based).
    cursor_line: usize,
    /// The cursor's line text, for the grapheme-column readout.
    line_text: &'a str,
    /// Byte column of the cursor within `line_text`.
    cursor_byte_col: usize,
    /// Grapheme count of the active selection (see [`PaintInputs::selected`]).
    selected: usize,
    /// Bar fill style (from the active theme).
    style: Style,
}

fn paint_status_bar(frame: &mut Frame, area: Rect, snapshot: &ViewSnapshot, status: StatusBar) {
    let col = layout::grapheme_column(status.line_text, status.cursor_byte_col);
    let (left, right) = layout::status_bar(
        status.cursor_line + 1,
        col,
        status.selected,
        snapshot.text.byte_len(),
        snapshot.version,
        snapshot.format,
        snapshot.read_only,
    );
    let bar = layout::fit_bar(&left, &right, area.width as usize);
    frame.render_widget(Paragraph::new(bar).style(status.style), area);
}

/// Which clickable part of the status bar a column falls on, for the snapshot the
/// bar was painted from.
///
/// A thin wrapper that assembles the same arguments `paint_status_bar` does, so the
/// hit test is asking about the bar that is actually on screen rather than about a
/// plausible-looking reconstruction of it.
fn status_target_at(
    snapshot: &ViewSnapshot,
    selected: usize,
    width: usize,
    column: usize,
) -> Option<layout::StatusTarget> {
    let head = snapshot
        .selections
        .get(snapshot.primary)
        .map(|s| s.head)
        .unwrap_or(0);
    let (cursor_line, cursor_byte_col, line_text) = layout::cursor_line_col(&snapshot.text, head);
    let col = layout::grapheme_column(&line_text, cursor_byte_col);
    let (left, _) = layout::status_bar(
        cursor_line + 1,
        col,
        selected,
        snapshot.text.byte_len(),
        snapshot.version,
        snapshot.format,
        snapshot.read_only,
    );
    layout::status_target(
        &left,
        snapshot.text.byte_len(),
        snapshot.version,
        snapshot.format,
        width,
        column,
    )
}

/// What a press or drag in the editor body means, given how many clicks the run has
/// reached (SPEC §2.2's pointer gestures).
///
/// Every branch resolves the pointer to a buffer offset and hands the *core* the
/// intent, never the coordinates - which is why this returns an `Action` rather than
/// doing anything: it is the whole screen→intent mapping for the body, and pure, so
/// the gesture table is testable without a terminal (SPEC §13). The event loop keeps
/// only the parts that need the outside world: the clock, and the send.
///
/// Precedence, highest first:
/// - **Alt** adds a cursor, whatever else is held - the multi-cursor gesture is not
///   something a click count should be able to turn into a word selection.
/// - **The gutter** selects the whole line. A line number is a line selector, not a
///   place to put a caret, and the click count is irrelevant there: clicking one
///   twice means what clicking it once means.
/// - **The click count** selects a word (2) or a line (3).
/// - Otherwise the caret moves, extending when dragged or shift-clicked.
fn press_action(
    snapshot: &ViewSnapshot,
    viewport: ViewState,
    tab_width: usize,
    mouse: MouseEvent,
    count: usize,
) -> Action {
    let is_press = matches!(mouse.kind, MouseEventKind::Down(_));
    let offset = pointer_offset(snapshot, viewport, tab_width, mouse.column, mouse.row);
    let gutter = layout::gutter_width(layout::display_line_count(&snapshot.text));
    let line = Action::SelectAround {
        offset,
        granularity: Granularity::Line,
    };

    if is_press && mouse.modifiers.contains(KeyModifiers::ALT) {
        return Action::AddCursorAt { offset };
    }
    if is_press && (mouse.column as usize) < gutter {
        return line;
    }
    match count {
        2 => Action::SelectAround {
            offset,
            granularity: Granularity::Word,
        },
        3 => line,
        _ => Action::PlaceCursor {
            offset,
            // A drag always extends, so a press-then-drag sweeps out a selection;
            // shift makes a press extend from the current anchor.
            extend: matches!(mouse.kind, MouseEventKind::Drag(_))
                || mouse.modifiers.contains(KeyModifiers::SHIFT),
        },
    }
}

/// RAII terminal setup/teardown: raw mode, alternate screen, and Kitty keyboard
/// flags. Guarantees the terminal is restored even on an error path - leaving a
/// user in raw mode is unacceptable (SPEC §8 spirit).
struct TerminalGuard {
    terminal: Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    kitty: bool,
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        ratatui::crossterm::terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(
            out,
            ratatui::crossterm::terminal::EnterAlternateScreen,
            // Report mouse press/drag/scroll so clicks place the caret and drags
            // select (SPEC §9 input). Disabled symmetrically on teardown.
            EnableMouseCapture,
            // Deliver an OS paste as a single `Event::Paste` (one `Insert` action,
            // SPEC §6) instead of a burst of synthetic keystrokes. Disabled on
            // teardown. Part of crossterm's default features (via ratatui).
            EnableBracketedPaste,
        )?;

        // Negotiate the Kitty keyboard protocol where supported (SPEC §9). A
        // terminal without it silently ignores the push, so we only enable when
        // detection succeeds to keep teardown symmetric.
        let kitty = supports_keyboard_enhancement().unwrap_or(false);
        if kitty {
            execute!(
                out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
        }

        let backend = ratatui::backend::CrosstermBackend::new(out);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;

        Ok(Self {
            terminal,
            kitty,
            active: true,
        })
    }

    /// Restore the terminal. Idempotent so an explicit call plus the `Drop`
    /// backstop do not double-restore. Best-effort: teardown errors are ignored
    /// because we are exiting anyway.
    fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut out = io::stdout();
        if self.kitty {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            out,
            DisableBracketedPaste,
            DisableMouseCapture,
            ratatui::crossterm::terminal::LeaveAlternateScreen
        );
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, row_text};
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    /// Drive the real core through an action script and return the resulting
    /// snapshot - the same seam a frontend uses (SPEC §1), so the chrome is
    /// rendered from a genuine `ViewSnapshot`, not a hand-built one (which
    /// `#[non_exhaustive]` forbids anyway). Runs the actor on an executor and
    /// awaits the final snapshot, exactly as the core's own interaction tests do.
    fn snapshot_after(script: &[Action]) -> ViewSnapshot {
        let ex = smol::Executor::new();
        let Core { handle, run } = vortex_core::new(64);
        ex.spawn(run).detach();
        smol::block_on(ex.run(async move {
            let mut snap = None;
            for action in script {
                handle.actions.send(action.clone()).await.unwrap();
                // Edits emit a delta before the snapshot; drain so the bounded
                // delta channel never blocks the actor across the script.
                while handle.deltas.try_recv().is_ok() {}
                snap = Some(handle.snapshots.recv().await.unwrap());
            }
            snap.expect("script must contain at least one action")
        }))
    }

    /// Drive `dispatch_command` against a live core, the way the event loop does.
    ///
    /// `setup` runs first (opening buffers, typing), then each command in `commands`
    /// is dispatched with a `Frontend` built from the latest snapshot - which is what
    /// makes buffer switching resolvable at all, since it reads that snapshot's list.
    /// Returns the final snapshot and the notifications the core emitted.
    fn dispatch_against_core(
        setup: &[Action],
        commands: &[Command],
    ) -> (ViewSnapshot, Vec<vortex_core::Notification>) {
        let (snap, notes, _) = dispatch_watching_overlays(setup, commands);
        (snap, notes)
    }

    /// [`dispatch_against_core`], additionally reporting whether an overlay was left
    /// open - for the commands whose whole point is whether a surface appears.
    fn dispatch_watching_overlays(
        setup: &[Action],
        commands: &[Command],
    ) -> (ViewSnapshot, Vec<vortex_core::Notification>, bool) {
        let ex = smol::Executor::new();
        let Core { handle, run } = vortex_core::new(64);
        ex.spawn(run).detach();
        smol::block_on(ex.run(async move {
            let mut latest = None;
            for action in setup {
                handle.actions.send(action.clone()).await.unwrap();
                while handle.deltas.try_recv().is_ok() {}
                latest = Some(handle.snapshots.recv().await.unwrap());
            }
            while handle.notifications.try_recv().is_ok() {}

            let mut config = config::Config::default();
            let mut overlays = Compositor::new();
            let mut toasts = Toasts::new(config.theme.toast_info, config.theme.toast_error);
            let mut search = buffersearch::SearchState::default();
            for command in commands {
                {
                    let mut ui = Frontend {
                        overlays: &mut overlays,
                        config: &mut config,
                        toasts: &mut toasts,
                        snapshot: latest.as_ref(),
                        search: &mut search,
                    };
                    assert!(dispatch_command(command.clone(), &handle, &mut ui));
                }
                // A dispatched core intent answers with a snapshot; a purely local
                // no-op (no neighbor to switch to) sends nothing, so poll for one
                // rather than awaiting - a bare `recv` would hang on that case.
                for _ in 0..64 {
                    smol::future::yield_now().await;
                    while handle.deltas.try_recv().is_ok() {}
                    if let Some(snap) = handle.snapshots.try_recv() {
                        latest = Some(snap);
                        break;
                    }
                }
            }
            let notes = std::iter::from_fn(|| handle.notifications.try_recv().ok()).collect();
            (
                latest.expect("a snapshot was produced"),
                notes,
                !overlays.is_empty(),
            )
        }))
    }

    /// Two temp files, opened as two buffers. Returns the dir (kept alive by the
    /// caller: dropping it deletes the files) and the open actions.
    fn two_open_files() -> (TempDir, Vec<Action>) {
        let dir = TempDir::new();
        let actions = ["one.txt", "two.txt"]
            .into_iter()
            .map(|name| {
                dir.file(name, &format!("body of {name}"));
                Action::Open(dir.path.join(name))
            })
            .collect();
        (dir, actions)
    }

    #[test]
    fn a_search_hit_opens_its_file_and_lands_on_the_match() {
        // The pick is an arrival, not an open: both actions go down the same channel
        // in order, so the jump resolves against the buffer the open just produced.
        // Nothing here waits for a snapshot in between, which is the point.
        let dir = TempDir::new();
        dir.file("hit.rs", "alpha\nbeta\nlet needle = 1;\n");
        let path = dir.path.join("hit.rs");
        let (snap, _) = dispatch_against_core(
            &[],
            &[
                Command::OpenAt {
                    path: path.clone(),
                    position: vortex_core::Position::new(2, 4),
                },
                // A second dispatch, so the poll takes the *jump's* snapshot rather
                // than the open's - two actions produce two of them.
                Command::Editor(Action::RequestSnapshot),
            ],
        );
        assert_eq!(snap.path.as_deref(), Some(path.as_path()));
        let caret = snap.selections[0].head;
        let text = snap.text.to_string();
        assert_eq!(
            &text[caret..caret + 6],
            "needle",
            "landed at {caret} in {text:?}"
        );
    }

    /// Render `snapshot` with a live search for `pattern` begun at `origin`, so a
    /// test can assert on what the highlights and the preview do to the frame.
    fn render_searching(
        snapshot: &ViewSnapshot,
        w: u16,
        h: u16,
        pattern: &str,
        origin: usize,
    ) -> ratatui::buffer::Buffer {
        let mut search = buffersearch::SearchState::default();
        search.begin(origin);
        search.refresh(
            buffersearch::Query::new(pattern.into(), String::new()),
            Some(&snapshot.text),
        );
        render_with(
            snapshot,
            w,
            h,
            PaintInputs {
                search: &search,
                ..paint_inputs(0)
            },
        )
    }

    /// The style of the cell at `(x, y)`.
    fn cell_style(buf: &ratatui::buffer::Buffer, x: u16, y: u16) -> Style {
        let cell = &buf[(x, y)];
        Style::new().fg(cell.fg).bg(cell.bg)
    }

    #[test]
    fn a_live_search_paints_every_visible_match_and_marks_the_current_one() {
        // The preview is what makes typing a query useful: the matches are marked
        // where they are, and the one Enter would take you to is marked differently,
        // because a screen of identical marks does not answer "which one is next".
        let snap = snapshot_after(&[
            Action::Insert("cat dog cat".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::BufferStart,
                extend: false,
            },
        ]);
        let theme = config::Theme::default();
        let buf = render_searching(&snap, 40, 5, "cat", 0);
        // Row 1 is the first text row; the gutter is 2 cells wide for a 1-line file.
        let gutter = layout::gutter_width(1) as u16;
        assert_eq!(
            cell_style(&buf, gutter, 1).bg,
            theme.search_current.bg,
            "the first match is the one the search is on"
        );
        assert_eq!(
            cell_style(&buf, gutter + 8, 1).bg,
            theme.search_match.bg,
            "the second is marked as a match, not as the current one"
        );
        // The cursor row carries the current-line tint, so "untouched" means neither
        // search style rather than the theme's plain ground.
        assert_eq!(
            cell_style(&buf, gutter + 4, 1).bg,
            theme.current_line.bg,
            "the text between them is untouched"
        );
    }

    #[test]
    fn nothing_is_painted_for_a_pattern_that_matches_nothing() {
        let snap = snapshot_after(&[Action::Insert("cat dog".into())]);
        let theme = config::Theme::default();
        let buf = render_searching(&snap, 40, 5, "zebra", 0);
        let gutter = layout::gutter_width(1) as u16;
        for x in gutter..gutter + 7 {
            // The cursor row's own tint, not either search style.
            assert_eq!(
                cell_style(&buf, x, 1).bg,
                theme.current_line.bg,
                "at column {x}"
            );
        }
    }

    #[test]
    fn the_viewport_follows_the_previewed_match_without_the_caret_moving() {
        // The half of the preview that is not a highlight: the view goes to the
        // match while the caret stays where it was, so cancelling the search puts
        // nothing back - there was nothing to put back.
        let mut body = "filler\n".repeat(80);
        body.push_str("needle here\n");
        let snap = snapshot_after(&[
            Action::Insert(body),
            Action::MoveCursor {
                motion: vortex_core::Motion::BufferStart,
                extend: false,
            },
        ]);
        assert_eq!(snap.selections[0].head, 0, "the caret is at the top");

        let buf = render_searching(&snap, 40, 10, "needle", 0);
        let rows: Vec<String> = (1..9).map(|y| row_text(&buf, y)).collect();
        assert!(
            rows.iter().any(|r| r.contains("needle")),
            "the match was scrolled into view: {rows:?}"
        );
    }

    #[test]
    fn escape_takes_down_the_highlights_a_committed_search_left_up() {
        // The prompt's own Escape is gone once a search has been committed, so the
        // editor's has to mean "done searching" as well as "one cursor" - otherwise
        // the highlights have no way off the screen.
        let (snap, _) = dispatch_against_core(
            &[Action::Insert("cat dog cat".into())],
            &[
                Command::PreviewSearch {
                    pattern: "cat".into(),
                    replacement: String::new(),
                },
                Command::FindNext,
                Command::Editor(Action::CollapseSelections),
            ],
        );
        // The search is gone from the frontend's memory; the buffer is untouched.
        assert_eq!(snap.text.to_string(), "cat dog cat");
    }

    #[test]
    fn find_next_walks_the_matches_of_the_previewed_pattern() {
        // The frontend is what remembers the pattern, so the repeat key fills it in
        // and only the complete `SelectNextMatch` crosses the seam (SPEC §7.5).
        let (snap, _) = dispatch_against_core(
            &[
                Action::Insert("cat dog cat".into()),
                Action::MoveCursor {
                    motion: vortex_core::Motion::BufferStart,
                    extend: false,
                },
            ],
            &[
                Command::PreviewSearch {
                    pattern: "cat".into(),
                    replacement: String::new(),
                },
                Command::FindNext,
                Command::FindNext,
            ],
        );
        let primary = snap.selections[snap.primary];
        assert_eq!(
            (primary.start(), primary.end()),
            (8, 11),
            "the second match is selected"
        );
    }

    #[test]
    fn find_next_before_any_search_is_a_no_op() {
        // Nothing to repeat means no round trip, not an empty-pattern search that
        // would match nowhere and cost a notification per keypress.
        let (snap, notes) = dispatch_against_core(
            &[Action::Insert("cat".into())],
            &[Command::FindNext, Command::SelectAllMatches],
        );
        assert_eq!(snap.text.to_string(), "cat");
        assert!(
            !notes
                .iter()
                .any(|n| matches!(n, vortex_core::Notification::SearchFailed { .. })),
            "{notes:?}"
        );
    }

    #[test]
    fn a_replace_walks_the_matches_and_edits_only_the_ones_agreed_to() {
        // The whole query-replace flow through the real dispatch: find, replace one,
        // skip one, replace the third.
        let (snap, _) = dispatch_against_core(
            &[
                Action::Insert("cat cat cat".into()),
                Action::MoveCursor {
                    motion: vortex_core::Motion::BufferStart,
                    extend: false,
                },
            ],
            &[
                Command::PreviewSearch {
                    pattern: "cat".into(),
                    replacement: "dog".into(),
                },
                // The walk starts by finding, so the first question is about a match
                // the user can see.
                Command::StartReplace,
                Command::Editor(Action::ReplaceMatch {
                    pattern: "cat".into(),
                    replacement: "dog".into(),
                }),
                Command::Editor(Action::SelectNextMatch {
                    pattern: "cat".into(),
                    backward: false,
                }),
                // ...skip the second.
                Command::Editor(Action::SelectNextMatch {
                    pattern: "cat".into(),
                    backward: false,
                }),
                Command::Editor(Action::ReplaceMatch {
                    pattern: "cat".into(),
                    replacement: "dog".into(),
                }),
            ],
        );
        assert_eq!(snap.text.to_string(), "dog cat dog");
    }

    #[test]
    fn a_replace_whose_pattern_matches_nothing_opens_no_walk() {
        // Regression: the walk used to open regardless, asking the user to agree to
        // replacing something that does not exist - and every answer fired another
        // "no matches" notification for the match it kept failing to find.
        let (snap, notes, overlay_open) = dispatch_watching_overlays(
            &[Action::Insert("cat dog".into())],
            &[
                Command::PreviewSearch {
                    pattern: "zebra".into(),
                    replacement: "X".into(),
                },
                Command::StartReplace,
            ],
        );
        assert!(!overlay_open, "the walk opened with nothing to walk");
        assert_eq!(snap.text.to_string(), "cat dog");
        // Exactly one "no matches", from the find the commit still sends - the user
        // is owed that much, and no more.
        assert_eq!(
            notes
                .iter()
                .filter(|n| matches!(n, vortex_core::Notification::SearchFailed { .. }))
                .count(),
            1,
            "{notes:?}"
        );
    }

    #[test]
    fn a_replace_with_matches_does_open_the_walk() {
        let (_, _, overlay_open) = dispatch_watching_overlays(
            &[
                Action::Insert("cat dog".into()),
                Action::MoveCursor {
                    motion: vortex_core::Motion::BufferStart,
                    extend: false,
                },
            ],
            &[
                Command::PreviewSearch {
                    pattern: "cat".into(),
                    replacement: "bird".into(),
                },
                Command::StartReplace,
            ],
        );
        assert!(overlay_open, "there was a match to ask about");
    }

    #[test]
    fn next_buffer_resolves_the_neighbor_from_the_snapshot_list() {
        // The core has no "next": the frontend reads the ordered list off the
        // snapshot, names the neighbor, and sends `SwitchBuffer { id }` (SPEC §7.5).
        let (_dir, opens) = two_open_files();
        let (snap, _) = dispatch_against_core(&opens, &[Command::NextBuffer]);
        // Two buffers, the second active after opening: next wraps to the first.
        assert_eq!(snap.buffers.len(), 2);
        assert_eq!(snap.buffer_id, snap.buffers[0].id);
        assert_eq!(snap.text.to_string(), "body of one.txt");

        // And back the other way.
        let (snap, _) = dispatch_against_core(&opens, &[Command::NextBuffer, Command::PrevBuffer]);
        assert_eq!(snap.buffer_id, snap.buffers[1].id);
    }

    #[test]
    fn buffer_switching_with_one_buffer_open_does_nothing() {
        // No neighbor: the key is a no-op rather than a round trip that re-selects
        // what is already on screen.
        let (snap, _) = dispatch_against_core(
            &[Action::Insert("only".into())],
            &[Command::NextBuffer, Command::PrevBuffer],
        );
        assert_eq!(snap.buffers.len(), 1);
        assert_eq!(snap.text.to_string(), "only");
    }

    #[test]
    fn close_buffer_sends_an_unforced_close_so_the_core_can_refuse() {
        // Clean buffer: it just closes.
        let (_dir, opens) = two_open_files();
        let (snap, _) = dispatch_against_core(&opens, &[Command::CloseBuffer]);
        assert_eq!(snap.buffers.len(), 1);

        // Dirty buffer: the core refuses, and that refusal is what raises the
        // confirmation in the event loop (SPEC §8).
        let mut dirty = opens.clone();
        dirty.push(Action::Insert("unsaved".into()));
        let (snap, notes) = dispatch_against_core(&dirty, &[Command::CloseBuffer]);
        assert_eq!(snap.buffers.len(), 2, "nothing was closed");
        assert!(
            notes
                .iter()
                .any(|n| matches!(n, vortex_core::Notification::CloseRejected { .. })),
            "expected the core to refuse, got {notes:?}"
        );
    }

    /// Default per-frame paint inputs for tests: fresh view state, default theme,
    /// caret-follow on, and the given selection count. Tests needing a different
    /// viewport or follow flag override via struct update syntax.
    fn paint_inputs(selected: usize) -> PaintInputs<'static> {
        // A shared empty search: most paint tests are not about searching, and
        // threading a borrow through every call site to say "no search" would be
        // noise. The tests that *are* about it build their own and call `paint`.
        static NO_SEARCH: std::sync::OnceLock<buffersearch::SearchState> =
            std::sync::OnceLock::new();
        PaintInputs {
            viewport: ViewState::default(),
            theme: config::Theme::default(),
            follow: true,
            selected,
            tab_width: config::DEFAULT_TAB_WIDTH,
            search: NO_SEARCH.get_or_init(buffersearch::SearchState::default),
        }
    }

    /// Render `snapshot` into an in-memory `TestBackend` of `w`x`h` cells via the
    /// real [`paint`] path, and hand back the painted buffer for cell assertions.
    /// The selection count is derived from the snapshot, as the event loop does.
    fn render(snapshot: &ViewSnapshot, w: u16, h: u16) -> ratatui::buffer::Buffer {
        render_with(snapshot, w, h, paint_inputs(0))
    }

    /// [`render`] with the per-frame inputs spelled out, for the settings that only
    /// show up in what gets painted.
    fn render_with(
        snapshot: &ViewSnapshot,
        w: u16,
        h: u16,
        inputs: PaintInputs,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let selected = layout::selected_grapheme_count(&snapshot.text, &snapshot.selections);
        terminal
            .draw(|frame| {
                paint(frame, snapshot, PaintInputs { selected, ..inputs });
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn the_configured_tab_width_is_what_gets_painted() {
        // The config value has to reach the paint path, not just parse: this is the
        // only place a tab stop is visible at all (SPEC §4, §10.5).
        let snap = snapshot_after(&[Action::Insert("a\tb".into())]);
        let gap = |width: usize| {
            let buf = render_with(
                &snap,
                40,
                5,
                PaintInputs {
                    tab_width: width,
                    ..paint_inputs(0)
                },
            );
            let row = row_text(&buf, 1);
            let a = row.find('a').expect("the line is painted");
            let b = row[a..].find('b').expect("both glyphs are painted");
            b - 1 // cells of tab between them
        };
        assert_eq!(
            gap(2),
            1,
            "a tab stop at 2 puts 'b' in the next column pair"
        );
        assert_eq!(gap(8), 7);
    }

    /// Parse a slice of string args (skipping argv[0]) the way `main` does.
    fn args(list: &[&str]) -> Args {
        parse_args(list.iter().map(OsString::from))
    }

    #[test]
    fn parse_args_no_args_opens_empty_buffer() {
        assert_eq!(
            args(&[]),
            Args::Open {
                file: None,
                config: None
            }
        );
    }

    #[test]
    fn parse_args_positional_is_the_file() {
        assert_eq!(
            args(&["notes.txt"]),
            Args::Open {
                file: Some(PathBuf::from("notes.txt")),
                config: None
            }
        );
    }

    #[test]
    fn parse_args_recognizes_help_and_version() {
        assert_eq!(args(&["--help"]), Args::Help);
        assert_eq!(args(&["-h"]), Args::Help);
        assert_eq!(args(&["--version"]), Args::Version);
        assert_eq!(args(&["-V"]), Args::Version);
    }

    #[test]
    fn parse_args_unknown_flag_is_not_opened_as_a_file() {
        // Regression: `vortex --frobnicate` must error, not open a buffer named
        // "--frobnicate" (and create that file on save).
        assert_eq!(
            args(&["--frobnicate"]),
            Args::Unknown(OsString::from("--frobnicate"))
        );
        assert_eq!(args(&["-x"]), Args::Unknown(OsString::from("-x")));
    }

    #[test]
    fn parse_args_double_dash_forces_following_arg_as_file() {
        // `--` ends option parsing so a file literally named "--version" opens.
        assert_eq!(
            args(&["--", "--version"]),
            Args::Open {
                file: Some(PathBuf::from("--version")),
                config: None
            }
        );
    }

    #[test]
    fn parse_args_lone_dash_is_treated_as_a_path() {
        // A bare "-" is a conventional stdin placeholder, not an unknown flag;
        // keep it as a path rather than erroring.
        assert_eq!(
            args(&["-"]),
            Args::Open {
                file: Some(PathBuf::from("-")),
                config: None
            }
        );
    }

    #[test]
    fn parse_args_first_positional_wins() {
        assert_eq!(
            args(&["a.txt", "b.txt"]),
            Args::Open {
                file: Some(PathBuf::from("a.txt")),
                config: None
            }
        );
    }

    /// A mouse event at a cell.
    fn mouse_at(
        kind: MouseEventKind,
        column: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers,
        }
    }

    fn press_at(column: u16, row: u16) -> MouseEvent {
        mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            KeyModifiers::NONE,
        )
    }

    /// The buffer the pointer-gesture tests aim at, with lines of distinct lengths
    /// so a selection's size says which line it came from.
    fn gesture_snapshot() -> ViewSnapshot {
        snapshot_after(&[Action::Insert(
            "alpha beta gamma\nsecond line here\nshort\n".into(),
        )])
    }

    /// A viewport that has been painted, unlike `ViewState::default()` - which
    /// reports a page height of 0 and so clamps every pointer row onto the first
    /// line. A real frame always has a height by the time a click arrives.
    fn painted_viewport() -> ViewState {
        ViewState {
            page_height: 20,
            ..ViewState::default()
        }
    }

    /// The text `action` would select, resolved against `snapshot` - so a test reads
    /// as the words and lines a user would see selected.
    fn selected_text(snapshot: &ViewSnapshot, action: &Action) -> String {
        match action {
            Action::SelectAround {
                offset,
                granularity,
            } => {
                let mut set = vortex_core::SelectionSet::at_origin();
                set.select_around(&snapshot.text, *offset, *granularity);
                let sel = set.primary();
                snapshot.text.to_string()[sel.start()..sel.end()].to_string()
            }
            other => panic!("expected a selection, got {other:?}"),
        }
    }

    #[test]
    fn one_click_places_the_caret_and_two_selects_the_word_under_it() {
        let snap = gesture_snapshot();
        let vp = painted_viewport();
        let single = press_action(&snap, vp, 4, press_at(5, 1), 1);
        assert!(
            matches!(single, Action::PlaceCursor { extend: false, .. }),
            "one click places a caret, got {single:?}"
        );
        let double = press_action(&snap, vp, 4, press_at(5, 1), 2);
        assert_eq!(selected_text(&snap, &double), "alpha");
    }

    #[test]
    fn three_clicks_select_the_line_the_pointer_is_on() {
        // Screen row 2 is the *second* body line - row 0 is the head bar - and the
        // whole line comes with it, terminator included.
        let snap = gesture_snapshot();
        let triple = press_action(&snap, painted_viewport(), 4, press_at(5, 2), 3);
        assert_eq!(selected_text(&snap, &triple), "second line here\n");
    }

    #[test]
    fn a_click_in_the_gutter_selects_that_line_whatever_the_count() {
        // A line number is a line selector: clicking one twice means what clicking
        // it once means, so the click run must not turn it into a word selection.
        let snap = gesture_snapshot();
        let vp = painted_viewport();
        for count in 1..=3 {
            let action = press_action(&snap, vp, 4, press_at(0, 3), count);
            assert_eq!(
                selected_text(&snap, &action),
                "short\n",
                "count {count} in the gutter"
            );
        }
    }

    #[test]
    fn alt_still_adds_a_cursor_whatever_the_count_says() {
        // The multi-cursor gesture outranks the click run: a fast Alt-click on the
        // same cell must not become a word selection.
        let snap = gesture_snapshot();
        let action = press_action(
            &snap,
            painted_viewport(),
            4,
            mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                5,
                1,
                KeyModifiers::ALT,
            ),
            2,
        );
        assert!(matches!(action, Action::AddCursorAt { .. }), "{action:?}");
    }

    #[test]
    fn a_drag_extends_and_never_selects() {
        // A drag is one continuous gesture. It reaches here with count 1 (the loop
        // resets the run), and dragging over the gutter must keep extending rather
        // than re-selecting a line and throwing the sweep away.
        let snap = gesture_snapshot();
        for column in [8, 0] {
            let drag = mouse_at(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                1,
                KeyModifiers::NONE,
            );
            let action = press_action(&snap, painted_viewport(), 4, drag, 1);
            assert!(
                matches!(action, Action::PlaceCursor { extend: true, .. }),
                "column {column}: {action:?}"
            );
        }
    }

    #[test]
    fn shift_click_extends_from_the_anchor() {
        let snap = gesture_snapshot();
        let action = press_action(
            &snap,
            painted_viewport(),
            4,
            mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                8,
                1,
                KeyModifiers::SHIFT,
            ),
            1,
        );
        assert!(
            matches!(action, Action::PlaceCursor { extend: true, .. }),
            "{action:?}"
        );
    }

    #[test]
    fn the_status_bars_format_words_resolve_to_their_pickers() {
        // The wrapper's job is to ask about the bar that is actually painted, which
        // means assembling the same left segment `paint_status_bar` does - the part
        // that decides whether the right segment fits at all.
        let snap = gesture_snapshot();
        let width = 80;
        let (_, right) = layout::status_bar(
            1,
            1,
            0,
            snap.text.byte_len(),
            snap.version,
            snap.format,
            snap.read_only,
        );
        let start = width - right.width();
        assert_eq!(
            status_target_at(&snap, 0, width, start),
            Some(layout::StatusTarget::Encoding)
        );
        assert_eq!(
            status_target_at(&snap, 0, width, start + right.width() - 2),
            None,
            "the version is a readout, not a control"
        );
    }

    #[test]
    fn a_terminal_too_narrow_for_the_metrics_offers_nothing_to_click() {
        // The left segment grows with a selection, and once it crowds the bar the
        // right segment is dropped - so there is nothing painted there to click.
        let snap = gesture_snapshot();
        for column in 0..24 {
            assert_eq!(
                status_target_at(&snap, 1234, 24, column),
                None,
                "column {column} on a crowded 24-cell bar"
            );
        }
    }

    #[test]
    fn parse_args_takes_a_config_path_in_either_spelling() {
        // The config has to be settled before the first frame, so it is a flag
        // rather than something the editor learns later (SPEC §10.5).
        let expected = Args::Open {
            file: Some(PathBuf::from("notes.txt")),
            config: Some(PathBuf::from("/etc/vortex.toml")),
        };
        assert_eq!(
            args(&["--config", "/etc/vortex.toml", "notes.txt"]),
            expected
        );
        assert_eq!(args(&["--config=/etc/vortex.toml", "notes.txt"]), expected);
    }

    #[test]
    fn parse_args_reports_a_config_flag_with_nothing_after_it() {
        // Silently starting on defaults would be the worst answer: the user asked
        // for a specific config and would never learn it was ignored.
        assert_eq!(args(&["--config"]), Args::MissingValue("--config"));
    }

    #[test]
    fn parse_args_does_not_read_a_config_value_as_a_flag() {
        // A config file named `--help` is absurd, but the value after the flag is a
        // path and must not be re-parsed as one.
        assert_eq!(
            args(&["--config", "--help"]),
            Args::Open {
                file: None,
                config: Some(PathBuf::from("--help"))
            }
        );
    }

    #[test]
    fn head_bar_shows_name_and_line_count_on_top_row() {
        let snap = snapshot_after(&[Action::Insert("a\nb\nc".into())]);
        let buf = render(&snap, 40, 10);
        let head = row_text(&buf, 0);
        assert!(head.contains(layout::NO_NAME), "head bar: {head:?}");
        assert!(head.contains("3 lines"), "head bar: {head:?}");
        // The row is painted with colour, not borders. The sole buffer's tab is the
        // active one, so it carries the accent fill; the bar's own ground shows
        // through beneath the line count. Asserted against the theme, not literals,
        // so a retheme is not a test edit.
        let theme = config::Theme::default();
        assert_eq!(
            buf.cell((0, 0)).unwrap().bg,
            theme.head_bar_active.bg.unwrap(),
            "the active tab is filled from the first cell"
        );
        assert_eq!(
            buf.cell((39, 0)).unwrap().bg,
            theme.head_bar.bg.unwrap(),
            "the bar's own ground runs to the right edge"
        );
    }

    #[test]
    fn the_bufferline_shows_a_tab_per_buffer_with_the_active_one_highlighted() {
        // The head bar is a tab strip once several buffers are open (SPEC §7.5:507).
        let (_dir, opens) = two_open_files();
        let snap = snapshot_after(&opens);
        assert_eq!(snap.buffers.len(), 2);
        let buf = render(&snap, 60, 10);
        let head = row_text(&buf, 0);
        assert!(head.contains("one.txt"), "bufferline: {head:?}");
        assert!(head.contains("two.txt"), "bufferline: {head:?}");
        // The line count keeps the right end of the bar.
        assert!(head.contains("line"), "bufferline: {head:?}");

        // Tabs are divided, not just spaced: two names must not read as one string.
        assert!(head.contains('│'), "bufferline: {head:?}");

        // The active tab is *filled* with the accent and the others recede, so the
        // current buffer survives a terminal where foreground brightness washes out.
        // Asserted against the theme rather than literals, so a retheme is not a test
        // edit.
        let theme = config::Theme::default();
        let column_of = |needle: &str| head.find(needle).expect("tab is present") as u16;
        let inactive = buf.cell((column_of("one.txt"), 0)).unwrap().clone();
        let active = buf.cell((column_of("two.txt"), 0)).unwrap().clone();
        assert_eq!(active.bg, theme.head_bar_active.bg.unwrap());
        assert_eq!(active.fg, theme.head_bar_active.fg.unwrap());
        assert_eq!(inactive.bg, theme.head_bar_inactive.bg.unwrap());
        assert_eq!(inactive.fg, theme.head_bar_inactive.fg.unwrap());
        // The distinction is carried by the background, not brightness alone.
        assert_ne!(active.bg, inactive.bg);

        // The divider is chrome: dimmer than either tab's text.
        let separator = buf.cell((head.find('│').unwrap() as u16, 0)).unwrap();
        assert_eq!(separator.fg, theme.head_bar_separator.fg.unwrap());
    }

    #[test]
    fn head_bar_shows_file_name_after_open() {
        // Open a real temp file; the head bar shows its file name (not full path).
        let dir = TempDir::new();
        let path = dir.path.join("greeting.txt");
        std::fs::write(&path, "hello").unwrap();

        let snap = snapshot_after(&[Action::Open(path.clone())]);
        let buf = render(&snap, 40, 10);
        let head = row_text(&buf, 0);
        assert!(head.contains("greeting.txt"), "head bar: {head:?}");
        // A freshly opened, unedited buffer is clean: no modified marker.
        assert!(!head.contains('●'), "head bar: {head:?}");
    }

    #[test]
    fn head_bar_shows_modified_marker_after_edit() {
        // Editing marks the buffer dirty; the head bar prefixes the name with ●.
        let snap = snapshot_after(&[Action::Insert("x".into())]);
        let buf = render(&snap, 40, 10);
        let head = row_text(&buf, 0);
        assert!(
            head.contains('●'),
            "head bar should show modified: {head:?}"
        );
    }

    #[test]
    fn status_bar_shows_cursor_position_on_bottom_row() {
        // Insert two lines, leaving the cursor at the end of line 2 (Ln 2, Col 4).
        let snap = snapshot_after(&[Action::Insert("ab\ncde".into())]);
        let buf = render(&snap, 40, 10);
        let status = row_text(&buf, 9); // bottom row
        assert!(status.contains("Ln 2, Col 4"), "status: {status:?}");
        assert!(status.contains("6B"), "status (byte count): {status:?}");
        assert!(status.contains("v1"), "status (version): {status:?}");
        let status_bg = config::Theme::default().status_bar.bg;
        assert_eq!(buf.cell((0, 9)).unwrap().bg, status_bg.unwrap());
    }

    /// Drive the core with a language server attached, feed one diagnostic batch
    /// from a fake server, and return the decorated snapshot - the frontend's only
    /// way to obtain a `ViewSnapshot` carrying decorations (it is
    /// `#[non_exhaustive]`, so it cannot be hand-built). Mirrors the core's own
    /// `drive_lsp` harness.
    fn snapshot_with_diagnostics(
        file: &std::path::Path,
        diagnostics: Vec<vortex_core::Diagnostic>,
    ) -> ViewSnapshot {
        use vortex_core::{DocumentSync, LspEvent, LspHandle};
        let ex = smol::Executor::new();
        let (sync_tx, sync_rx) = async_channel::bounded::<DocumentSync>(16);
        let (event_tx, event_rx) = async_channel::bounded::<LspEvent>(16);
        let Core { handle, run } = vortex_core::with_lsp(
            64,
            LspHandle {
                sync: sync_tx,
                events: event_rx,
            },
        );
        ex.spawn(run).detach();
        smol::block_on(ex.run(async move {
            handle
                .actions
                .send(Action::Open(file.to_path_buf()))
                .await
                .unwrap();
            while handle.deltas.try_recv().is_ok() {}
            handle.snapshots.recv().await.unwrap();
            // Keep the sync channel drained so the actor never blocks on it.
            while sync_rx.try_recv().is_ok() {}
            event_tx
                .send(LspEvent::Diagnostics {
                    path: file.to_path_buf(),
                    diagnostics,
                })
                .await
                .unwrap();
            handle.snapshots.recv().await.unwrap()
        }))
    }

    fn error_at(line: usize, start: usize, end: usize) -> vortex_core::Diagnostic {
        vortex_core::Diagnostic {
            start: vortex_core::Utf16Position::new(line, start),
            end: vortex_core::Utf16Position::new(line, end),
            severity: vortex_core::Severity::Error,
            message: "mismatched types".into(),
        }
    }

    #[test]
    fn a_diagnostic_underlines_its_span_with_the_severity_color() {
        // The TUI half of M2's criterion: the span rust-analyzer flagged is painted
        // underlined in the error color. Fixture "let x = y" with an error over "y".
        let dir = TempDir::new();
        let path = dir.path.join("a.rs");
        std::fs::write(&path, "let x = y").unwrap();
        // "y" is the 9th column (chars 8..9), one ASCII byte so UTF-16 == byte here.
        let snap = snapshot_with_diagnostics(&path, vec![error_at(0, 8, 9)]);
        let buf = render(&snap, 40, 6);

        // Row 1 is the first body row; the gutter is "  1 " (4 cells), so text
        // column 8 lands at cell 4 + 8 = 12, painting "y".
        let cell = buf.cell((12, 1)).unwrap();
        assert_eq!(cell.symbol(), "y", "the underline should sit on `y`");
        assert_eq!(
            cell.fg,
            config::Theme::default().diagnostic_error.fg.unwrap(),
            "the span is painted in the error color"
        );
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "a diagnostic span is underlined"
        );
    }

    /// Drive the core with a fake highlighter attached, run `script`, then push one
    /// highlight batch and return the decorated snapshot - the syntax twin of
    /// [`snapshot_with_diagnostics`]. `spans` are applied against the version the
    /// script left the buffer at.
    fn snapshot_with_highlights(
        script: &[Action],
        spans: Vec<vortex_core::HighlightSpan>,
    ) -> ViewSnapshot {
        use vortex_core::{SyntaxEvent, SyntaxHandle, SyntaxSync};
        let ex = smol::Executor::new();
        let (sync_tx, sync_rx) = async_channel::bounded::<SyntaxSync>(16);
        let (event_tx, event_rx) = async_channel::bounded::<SyntaxEvent>(16);
        let Core { handle, run } = vortex_core::new(64);
        ex.spawn(run).detach();
        smol::block_on(ex.run(async move {
            handle
                .syntax
                .send(SyntaxHandle {
                    sync: sync_tx,
                    events: event_rx,
                })
                .await
                .unwrap();
            let mut snap = None;
            for action in script {
                handle.actions.send(action.clone()).await.unwrap();
                while handle.deltas.try_recv().is_ok() {}
                snap = Some(handle.snapshots.recv().await.unwrap());
            }
            // The batch has to name the buffer *and* version it was parsed against,
            // or the core drops it as stale (versions alone are per-buffer).
            let snap = snap.expect("script must contain at least one action");
            let (buffer_id, version) = (snap.buffer_id, snap.version);
            // Keep the highlighter's sync channel drained so the actor never blocks.
            while sync_rx.try_recv().is_ok() {}
            event_tx
                .send(SyntaxEvent::Highlights {
                    buffer_id,
                    version,
                    spans,
                })
                .await
                .unwrap();
            handle.snapshots.recv().await.unwrap()
        }))
    }

    fn highlight_span(
        range: std::ops::Range<usize>,
        kind: vortex_core::HighlightKind,
    ) -> vortex_core::HighlightSpan {
        vortex_core::HighlightSpan { range, kind }
    }

    #[test]
    fn a_syntax_highlight_paints_its_span_with_the_role_color() {
        // "fn x" with a keyword highlight over "fn": the glyphs take the keyword
        // color (M4). Gutter "  1 " is 4 cells, so "f" lands at cell 4.
        let snap = snapshot_with_highlights(
            &[Action::Insert("fn x".into())],
            vec![highlight_span(0..2, vortex_core::HighlightKind::Keyword)],
        );
        let buf = render(&snap, 40, 6);
        let cell = buf.cell((4, 1)).unwrap();
        assert_eq!(cell.symbol(), "f", "the highlight should sit on `fn`");
        assert_eq!(
            cell.fg,
            config::Theme::default().syntax_keyword.fg.unwrap(),
            "a keyword span is painted in the keyword color"
        );
    }

    #[test]
    fn multiple_highlights_on_a_line_paint_at_their_own_columns_past_a_wide_char() {
        // The shared-walker path (one ColumnWalker resolves every span on a line):
        // two sorted spans separated by a wide char must each land at the right
        // cell, proving the walker tracks display columns across the 2-cell glyph
        // rather than drifting. "fn 日x": "fn" (keyword) at cols 0-1, "日" (2 cells)
        // at cols 3-4, "x" (type) at col 5. Gutter "  1 " is 4 cells.
        let snap = snapshot_with_highlights(
            &[Action::Insert("fn 日x".into())],
            vec![
                highlight_span(0..2, vortex_core::HighlightKind::Keyword),
                highlight_span(6..7, vortex_core::HighlightKind::Type),
            ],
        );
        let buf = render(&snap, 40, 6);
        let kw = buf.cell((4, 1)).unwrap();
        assert_eq!(kw.symbol(), "f");
        assert_eq!(
            kw.fg,
            config::Theme::default().syntax_keyword.fg.unwrap(),
            "the first span keeps the keyword color"
        );
        // The second span sits at gutter 4 + col 5 = cell 9, after the wide glyph.
        let ty = buf.cell((9, 1)).unwrap();
        assert_eq!(ty.symbol(), "x");
        assert_eq!(
            ty.fg,
            config::Theme::default().syntax_type.fg.unwrap(),
            "the second span resolves to the correct column past the wide char"
        );
    }

    #[test]
    fn a_selected_highlight_keeps_its_syntax_color_on_the_selection_ground() {
        // The behavior selection was reordered under highlights for: selecting the
        // whole line leaves `fn` its keyword color, on the selection's background -
        // syntax is not flattened to the selection's own foreground.
        let snap = snapshot_with_highlights(
            &[
                Action::Insert("fn x".into()),
                Action::PlaceCursor {
                    offset: 0,
                    extend: true,
                },
            ],
            vec![highlight_span(0..2, vortex_core::HighlightKind::Keyword)],
        );
        let buf = render(&snap, 40, 6);
        let cell = buf.cell((4, 1)).unwrap();
        assert_eq!(cell.symbol(), "f");
        assert_eq!(
            cell.fg,
            config::Theme::default().syntax_keyword.fg.unwrap(),
            "the syntax color survives the selection"
        );
        assert_eq!(
            cell.bg,
            config::Theme::default().selection.bg.unwrap(),
            "on the selection's background"
        );
    }

    #[test]
    fn ensure_ignores_a_file_with_no_grammar() {
        // A file type with no grammar attaches nothing and leaves the manager empty,
        // the frontend degrading to no highlighting (SPEC §8).
        let Core { handle, run: _run } = vortex_core::new(16);
        let mut manager = GrammarManager::new();
        manager.ensure(Path::new("notes.txt"), &handle);
        assert_eq!(manager.current, None);
    }

    #[test]
    fn ensure_reports_whether_it_started_an_attach() {
        // The return value is what arms the paint hold: only a real attach means a
        // fresh highlight batch is coming, and holding the frame for one that is not
        // on its way stalls a buffer switch for the whole `HIGHLIGHT_WAIT`.
        //
        // It also drives the stale-attach guard, which counts one generation per
        // attach. The *other* half of that guard - a slow loader finding its
        // generation superseded and dropping its grammar - needs two languages to
        // race, and `grammar_target` knows only Rust today, so it cannot be reached
        // from a test; `ensure` is in the documented-untestable `dlopen` glue
        // (CLAUDE.md) for the same reason.
        let Core { handle, run: _run } = vortex_core::new(16);
        let mut manager = GrammarManager::new();

        // Nothing to attach for a file type with no grammar, so nothing to wait for
        // and no generation spent.
        assert!(!manager.ensure(Path::new("notes.txt"), &handle));
        assert_eq!(manager.generation.load(Ordering::SeqCst), 0);

        // A language with a grammar: an attach starts and claims a generation.
        assert!(manager.ensure(Path::new("a.rs"), &handle));
        assert_eq!(manager.current, Some("rust"));
        assert_eq!(manager.generation.load(Ordering::SeqCst), 1);

        // Another file of the same language reuses the running highlighter. No second
        // attach, and critically no second generation - bumping here would invalidate
        // the in-flight loader that is about to serve this very buffer.
        assert!(!manager.ensure(Path::new("b.rs"), &handle));
        assert_eq!(manager.generation.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_diagnostic_recolors_its_lines_gutter_number() {
        let dir = TempDir::new();
        let path = dir.path.join("a.rs");
        std::fs::write(&path, "ok\nbad line").unwrap();
        // An error on line 1 (the second line): chars 0..3 over "bad".
        let snap = snapshot_with_diagnostics(&path, vec![error_at(1, 0, 3)]);
        let buf = render(&snap, 40, 6);

        // Line 1's number renders in the gutter of body row 2. Its digit cell must
        // carry the error color; line 0's gutter must not.
        let err = config::Theme::default().diagnostic_error.fg.unwrap();
        let marked = buf.cell((2, 2)).unwrap(); // "2" of line 2's "  2 "
        assert_eq!(marked.symbol(), "2");
        assert_eq!(marked.fg, err, "the flagged line's gutter takes the color");
        let clean = buf.cell((2, 1)).unwrap(); // "1" of line 1
        assert_ne!(clean.fg, err, "an unflagged line's gutter is untouched");
    }

    #[test]
    fn a_buffer_with_no_diagnostics_paints_no_underline() {
        // The common no-LSP path must be visibly unchanged: no cell is underlined.
        let snap = snapshot_after(&[Action::Insert("let x = y".into())]);
        let buf = render(&snap, 40, 6);
        for x in 0..40 {
            assert!(
                !buf.cell((x, 1))
                    .unwrap()
                    .modifier
                    .contains(Modifier::UNDERLINED),
                "cell {x} should not be underlined without a diagnostic"
            );
        }
    }

    #[test]
    fn lsp_target_declines_unknown_extensions() {
        // A file type with no server entry attaches nothing - the editor runs with
        // no diagnostics rather than failing.
        assert!(lsp_target(Path::new("notes.txt")).is_none());
        assert!(lsp_target(Path::new("Makefile")).is_none());
    }

    #[test]
    fn lsp_target_declines_when_the_server_is_not_installed() {
        // A `.rs` file only attaches if rust-analyzer is actually on PATH; the
        // probe returns false for a name that cannot resolve, so this is hermetic
        // whether or not rust-analyzer is installed on the test machine.
        assert!(!server_on_path("vortex-definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn lsp_manager_attaches_a_server_only_once_per_workspace() {
        // The dedup that stops the picker relaunching rust-analyzer on every open:
        // a second ensure for the same (command, root) must not attach again. Driven
        // through the real handle so `send_blocking` has a live receiver.
        let Core { handle, run } = vortex_core::new(4);
        let ex = smol::Executor::new();
        ex.spawn(run).detach();
        // Seed the attached set as if a server for this cwd were already running,
        // then confirm a repeat ensure is a no-op (no new thread, no send). Uses a
        // fabricated command so nothing is actually spawned.
        let mut mgr = LspManager::new();
        let root = std::env::current_dir().unwrap();
        mgr.attached.insert(("rust-analyzer", root.clone()));
        // A `.rs` path in this cwd resolves to the already-attached pair, so ensure
        // returns without touching the core.
        mgr.ensure(Path::new("already_open.rs"), &handle);
        assert!(handle.lsp.is_empty(), "no second attach should be sent");
        drop(handle);
        drop(ex);
    }

    #[test]
    fn gutter_numbers_lines_from_one() {
        let snap = snapshot_after(&[Action::Insert("first\nsecond".into())]);
        let buf = render(&snap, 40, 10);
        // Body starts at row 1 (row 0 is the head bar). Gutter is 3-digit field +
        // space; line 1 renders "  1 " then the text.
        let row1 = row_text(&buf, 1);
        let row2 = row_text(&buf, 2);
        assert!(row1.starts_with("  1 first"), "row1: {row1:?}");
        assert!(row2.starts_with("  2 second"), "row2: {row2:?}");
    }

    #[test]
    fn long_line_scrolls_horizontally_to_follow_cursor() {
        // A line wider than the viewport: after typing, the cursor is at the far
        // right, so paint scrolls right - the leading characters are clipped and
        // the cursor stays on screen. Width 12 = 4-cell gutter + 8 text cells.
        let snap = snapshot_after(&[Action::Insert("abcdefghijklmnop".into())]);
        let buf = render(&snap, 12, 4);
        let row = row_text(&buf, 1); // first body row
        // Gutter still shows line 1 (gutter never scrolls horizontally).
        assert!(row.starts_with("  1 "), "gutter should be fixed: {row:?}");
        // The start of the line ("abc") is scrolled off; the tail ("...nop") shows.
        assert!(
            !row.contains("abc"),
            "leading text should be clipped: {row:?}"
        );
        assert!(row.contains("nop"), "cursor end should be visible: {row:?}");
    }

    #[test]
    fn cursor_stays_within_viewport_on_a_long_line() {
        // The terminal cursor must land inside the visible area, not off the right
        // edge, once horizontal scroll follows it.
        let snap = snapshot_after(&[Action::Insert("0123456789abcdef".into())]);
        let mut terminal = Terminal::new(TestBackend::new(12, 4)).unwrap();
        terminal
            .draw(|frame| {
                paint(frame, &snap, paint_inputs(0));
            })
            .unwrap();
        let pos = terminal.backend().cursor_position();
        // x must be within [gutter, width): visible, not overflowing to column 12+.
        assert!(pos.x < 12, "cursor x {} should be on screen", pos.x);
        assert!(pos.x >= 4, "cursor x {} should be past the gutter", pos.x);
    }

    #[test]
    fn home_scrolls_back_to_the_line_start() {
        // End then Home on a long line: Home moves the cursor to col 0, and the
        // horizontal scroll follows back so the line start is visible again.
        // (End/Home need no dedicated code - scroll-follow does the work.)
        let script = &[
            Action::Insert("abcdefghijklmnop".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::LineStart,
                extend: false,
            },
        ];
        let snap = snapshot_after(script);
        let buf = render(&snap, 12, 4);
        let row = row_text(&buf, 1);
        assert!(
            row.starts_with("  1 abc"),
            "line start should show: {row:?}"
        );
    }

    #[test]
    fn cursor_line_gutter_is_emphasized() {
        // Cursor ends on line 2; its gutter number is bold+white, the other dim.
        let snap = snapshot_after(&[Action::Insert("x\ny".into())]);
        let buf = render(&snap, 40, 10);
        // The '1' digit sits in column 2 of the 4-wide gutter ("  1 ").
        let theme = config::Theme::default();
        let inactive = buf.cell((2, 1)).unwrap();
        let active = buf.cell((2, 2)).unwrap();
        assert_eq!(inactive.fg, theme.gutter.fg.unwrap());
        assert_eq!(active.fg, theme.gutter_current.fg.unwrap());
        assert!(active.modifier.contains(Modifier::BOLD));
        // The two must actually differ, or "emphasized" means nothing.
        assert_ne!(inactive.fg, active.fg);
    }

    #[test]
    fn selection_is_highlighted_across_its_span() {
        // Type a word, then select it back to the line start (Shift+Home). The
        // selected cells carry the selection background; cells past it do not.
        let snap = snapshot_after(&[
            Action::Insert("hello".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::LineStart,
                extend: true,
            },
        ]);
        let buf = render(&snap, 40, 10);
        let sel = config::Theme::default().selection;
        // Gutter is 4 cells; "hello" occupies text columns 4..9 on body row 1.
        assert_eq!(buf.cell((4, 1)).unwrap().bg, sel.bg.unwrap());
        assert_eq!(buf.cell((8, 1)).unwrap().bg, sel.bg.unwrap());
        // Selected text carries the selection's contrasting foreground.
        assert_eq!(buf.cell((4, 1)).unwrap().fg, sel.fg.unwrap());
        // A cell past the selected text is not part of the selection.
        assert_ne!(buf.cell((20, 1)).unwrap().bg, sel.bg.unwrap());
    }

    #[test]
    fn secondary_caret_is_painted_as_a_reversed_cell() {
        // Multi-cursor: type two lines, go to the top, add a cursor below. The new
        // caret (line 1) is primary and shows as the terminal cursor; the caret left
        // on line 0 is secondary and must be visible as a one-cell reversed block.
        let snap = snapshot_after(&[
            Action::Insert("ab\ncd".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::BufferStart,
                extend: false,
            },
            Action::AddCursorBelow,
        ]);
        assert_eq!(snap.selections.len(), 2, "two carets");
        let buf = render(&snap, 40, 10);
        // Secondary caret at line 0, col 0 -> body row 1, screen col 4 (past the
        // 4-cell gutter). It carries the theme's secondary-cursor marker, whatever
        // that theme expresses it as (a block, reversed video, …).
        let marker = config::Theme::default().secondary_cursor;
        let cell = buf.cell((4, 1)).unwrap();
        assert_eq!(cell.bg, marker.bg.unwrap(), "secondary caret is marked");
        assert_eq!(cell.fg, marker.fg.unwrap());
    }

    #[test]
    fn cursor_line_is_tinted_full_width() {
        // Two lines, cursor left on line 2: that whole row (including padding past
        // the text) gets the current-line tint; the other line does not.
        let snap = snapshot_after(&[Action::Insert("ab\ncd".into())]);
        let buf = render(&snap, 40, 10);
        // Body row 1 = line 1, row 2 = line 2 (the cursor line).
        let tint = config::Theme::default().current_line.bg.unwrap();
        assert_eq!(buf.cell((30, 2)).unwrap().bg, tint);
        assert_ne!(buf.cell((30, 1)).unwrap().bg, tint);
    }

    #[test]
    fn status_bar_shows_selection_count_when_active() {
        let snap = snapshot_after(&[
            Action::Insert("hello".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::LineStart,
                extend: true,
            },
        ]);
        let buf = render(&snap, 40, 10);
        let status = row_text(&buf, 9);
        assert!(status.contains("(5 selected)"), "status: {status:?}");
    }

    #[test]
    fn cursor_sits_after_the_gutter() {
        // Fresh empty buffer: cursor at Ln 1 Col 1, painted just right of the
        // 4-cell gutter on the first body row (row 1).
        let snap = snapshot_after(&[Action::RequestSnapshot]);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                paint(frame, &snap, paint_inputs(0));
            })
            .unwrap();
        let pos = terminal.backend().cursor_position();
        assert_eq!((pos.x, pos.y), (4, 1));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        // A terminal too short for head + status + any body must still render
        // (SPEC §8: a degenerate resize must never crash).
        let snap = snapshot_after(&[Action::Insert("hello".into())]);
        let _ = render(&snap, 4, 2);
        let _ = render(&snap, 1, 1);
    }

    #[test]
    fn empty_buffer_shows_line_one_in_gutter() {
        // Regression: a fresh empty buffer must paint gutter number "1" and the
        // head bar must read "1 line" - not a blank body with no numbers.
        let snap = snapshot_after(&[Action::RequestSnapshot]);
        let buf = render(&snap, 40, 10);
        assert!(
            row_text(&buf, 0).contains("1 line"),
            "head: {:?}",
            row_text(&buf, 0)
        );
        assert!(
            row_text(&buf, 1).starts_with("  1 "),
            "row1: {:?}",
            row_text(&buf, 1)
        );
    }

    #[test]
    fn trailing_newline_gets_its_own_numbered_row() {
        // Regression (user report): pressing Enter at end of file must show the new
        // empty line with its own gutter number, not swallow it as a terminator.
        let snap = snapshot_after(&[Action::Insert("hi\n".into())]);
        let buf = render(&snap, 40, 10);
        assert!(
            row_text(&buf, 1).starts_with("  1 hi"),
            "row1: {:?}",
            row_text(&buf, 1)
        );
        // Line 2 is blank but still numbered "2".
        assert!(
            row_text(&buf, 2).starts_with("  2 "),
            "row2: {:?}",
            row_text(&buf, 2)
        );
        assert!(
            row_text(&buf, 0).contains("2 lines"),
            "head: {:?}",
            row_text(&buf, 0)
        );
    }

    #[test]
    fn stale_vertical_scroll_is_clamped_to_content_height() {
        // A viewport carried from a taller buffer must not keep the top scrolled past
        // the content after the buffer shrinks: `scroll_to_show` only pulls the offset
        // down to the cursor line, not to a full screen of content, so without the
        // clamp lines above the cursor that would fit stay hidden behind blank rows.
        let snap = snapshot_after(&[Action::Insert("l0\nl1\nl2".into())]); // 3 lines
        let mut settled = ViewState::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        // Body = 6 - 2 bars = 4 rows; all 3 lines fit, so the only valid top is 0.
        let stale = ViewState {
            scroll: 50,
            h_scroll: 0,
            page_height: 4,
        };
        terminal
            .draw(|frame| {
                settled = paint(
                    frame,
                    &snap,
                    PaintInputs {
                        viewport: stale,
                        ..paint_inputs(0)
                    },
                )
            })
            .unwrap();
        assert_eq!(settled.scroll, 0, "scroll must clamp to fit the content");
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 1).contains("l0"),
            "top line should be visible, not scrolled off: {:?}",
            row_text(&buf, 1)
        );
    }

    #[test]
    fn stale_horizontal_scroll_is_clamped_to_line_width() {
        // A horizontal offset carried from a long line must clamp to the current
        // (short) line's width once the cursor moves onto it, so the line is shown
        // from the left instead of scrolled off the right edge into blank cells.
        let snap = snapshot_after(&[Action::Insert("hi".into())]); // 2-wide line, caret at col 2
        let mut settled = ViewState::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        let stale = ViewState {
            scroll: 0,
            h_scroll: 40,
            page_height: 2,
        };
        terminal
            .draw(|frame| {
                settled = paint(
                    frame,
                    &snap,
                    PaintInputs {
                        viewport: stale,
                        ..paint_inputs(0)
                    },
                )
            })
            .unwrap();
        assert_eq!(settled.h_scroll, 0, "h_scroll must clamp to the short line");
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 1).contains("hi"),
            "the line should be visible from the left: {:?}",
            row_text(&buf, 1)
        );
    }

    #[test]
    fn wheel_scroll_moves_view_without_following_the_caret() {
        // Six lines, caret pinned to the top (line 0). With follow off (a wheel
        // scroll) the view honors the given scroll offset instead of snapping back
        // to the caret, so lower lines show and the caret scrolls out of sight.
        let snap = snapshot_after(&[
            Action::Insert("l0\nl1\nl2\nl3\nl4\nl5".into()),
            Action::MoveCursor {
                motion: vortex_core::Motion::BufferStart,
                extend: false,
            },
        ]);
        // 6 rows - 2 bars = 4 text rows; scroll down to line 2.
        let scrolled = ViewState {
            scroll: 2,
            h_scroll: 0,
            page_height: 4,
        };
        let mut settled = ViewState::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|frame| {
                settled = paint(
                    frame,
                    &snap,
                    PaintInputs {
                        viewport: scrolled,
                        follow: false,
                        ..paint_inputs(0)
                    },
                )
            })
            .unwrap();
        // The view stayed scrolled (not pulled back to the caret on line 0).
        assert_eq!(settled.scroll, 2);
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 1).contains("l2"),
            "top body row should be the scrolled line: {:?}",
            row_text(&buf, 1)
        );
    }

    #[test]
    fn pointer_offset_subtracts_the_head_bar_row() {
        let snap = snapshot_after(&[Action::Insert("ab\ncdef".into())]);
        let vp = ViewState {
            scroll: 0,
            h_scroll: 0,
            page_height: 8,
        };
        // Screen row 2 is body row 1 = line "cdef" (starts at byte 3); the gutter
        // edge (column 4) maps to its first character.
        assert_eq!(
            pointer_offset(&snap, vp, config::DEFAULT_TAB_WIDTH, 4, 2),
            3
        );
        // A click on the head bar (screen row 0) clamps to the top line's start.
        assert_eq!(
            pointer_offset(&snap, vp, config::DEFAULT_TAB_WIDTH, 4, 0),
            0
        );
    }
}
