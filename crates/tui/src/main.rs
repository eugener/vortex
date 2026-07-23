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

mod command;
mod compositor;
mod config;
mod filepicker;
mod grammar;
mod keymap;
mod layout;
mod osc52;
mod palette;
mod picker;
#[cfg(test)]
mod testutil;
mod theme;
mod themepicker;
mod toast;

use std::ffi::OsString;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

use vortex_core::{Action, Core, ViewSnapshot};

use command::Command;
use compositor::{Compositor, EventResult};
use toast::Toasts;

/// Default tab stop width for display-column layout (SPEC §4). Config in M5.
const TAB_WIDTH: usize = 4;

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

/// Lines the mouse wheel scrolls the viewport per notch. A few lines per notch is
/// the common terminal feel; scrolling is a pure frontend viewport move (SPEC §5),
/// so it never round-trips to the core.
const SCROLL_STEP: usize = 3;

fn main() -> io::Result<()> {
    // Parse argv before touching the terminal: `--help`/`--version` and bad flags
    // must print to normal stdout/stderr, not paint into the alternate screen.
    let path = match parse_args(std::env::args_os().skip(1)) {
        Args::Open(path) => path,
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

    // Resolve frontend configuration once, up front. Today this is the built-in
    // default; M5 swaps it for `Config::load` reading the user's file (SPEC §10.5).
    // Parsed here, next to argv, because that is where a `--config <path>` flag will
    // live and because config must be settled before the first frame paints.
    let config = config::Config::default();

    // Terminal setup. On any error we still attempt teardown so we never leave the
    // user's terminal in raw mode (the Drop impl is the backstop).
    let mut term = TerminalGuard::enter()?;
    let result = event_loop(
        &handle,
        &mut term.terminal,
        path,
        config,
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
}

impl GrammarManager {
    fn new() -> Self {
        Self { current: None }
    }

    /// Ensure the highlighter attached to the core matches `path`'s language,
    /// loading and attaching its grammar if it differs from the current one. A file
    /// type with no grammar, a missing library, or a load failure leaves the editor
    /// running with no fresh highlights (SPEC §8) - never crashing. The highlighter
    /// loop runs on its own thread, off the render thread, exactly like an LSP
    /// client.
    fn ensure(&mut self, path: &Path, handle: &vortex_core::CoreHandle) {
        let Some(lang) = grammar::grammar_target(path) else {
            return;
        };
        // Same language as the running highlighter: its resync already covers the
        // newly opened file, so do not reload the grammar.
        if self.current == Some(lang) {
            return;
        }
        let Some(resolved) = grammar::resolve(lang) else {
            return;
        };
        let Some(language) = load_grammar(&resolved.lib_path) else {
            return;
        };
        let (syntax_handle, syntax_loop) = vortex_core::highlighter(
            language,
            lang,
            resolved.highlights,
            resolved.injections,
            String::new(),
        );
        // The loop resolves to why it stopped; a query-compile failure is swallowed
        // rather than crashing the editor (SPEC §8).
        let spawned = std::thread::Builder::new()
            .name("vortex-syntax".into())
            .spawn(move || {
                let _ = smol::block_on(syntax_loop);
            });
        if spawned.is_err() {
            return;
        }
        // A closed channel means the core has stopped; nothing to attach to.
        if handle.syntax.send_blocking(syntax_handle).is_ok() {
            self.current = Some(lang);
        }
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
      --           Treat every following argument as a file name

Keys:
  Ctrl+S           Save        Ctrl+Q            Quit
  Ctrl+O           Open file (fuzzy picker over the working directory)
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
    /// Open this file, or start an empty unnamed buffer (`None`).
    Open(Option<PathBuf>),
    Help,
    Version,
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
    let mut flags_done = false;
    for arg in args {
        if flags_done {
            file.get_or_insert_with(|| PathBuf::from(&arg));
            continue;
        }
        match arg.to_str() {
            Some("--") => flags_done = true,
            Some("-h" | "--help") => return Args::Help,
            Some("-V" | "--version") => return Args::Version,
            // A dashed token we do not recognize (but not a lone "-", which is a
            // conventional stdin placeholder / valid-ish name, left as a path).
            Some(s) if s.starts_with('-') && s != "-" => return Args::Unknown(arg),
            // First positional wins; extra files are ignored until multi-buffer.
            _ => {
                file.get_or_insert_with(|| PathBuf::from(&arg));
            }
        }
    }
    Args::Open(file)
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
    // Transient file/edit notices (open/save results, failures) surface here as
    // top-right toasts that auto-fade, rather than hijacking the status bar (SPEC
    // §7.5). A failed save must be visible, not silent (SPEC §8).
    let mut toasts = Toasts::new(config.theme.toast_info, config.theme.toast_error);
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
            if let vortex_core::Notification::FileOpened { path, .. } = &note {
                lsp.ensure(path, handle);
                grammars.ensure(path, handle);
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

        if let Some(snap) = &latest
            && needs_redraw
        {
            viewport = draw(
                terminal,
                snap,
                PaintInputs {
                    viewport,
                    theme: config.theme,
                    follow,
                    selected,
                },
                &overlays,
                &toasts,
            )?;
            needs_redraw = false;
            // Default back to caret-follow; only a wheel scroll opts out, and only
            // for the single frame it triggered.
            follow = true;
        }

        // Wait for input, but no longer than POLL so a snapshot arriving without a
        // keystroke still gets painted on the next tick.
        if event::poll(POLL)? {
            match event::read()? {
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
                        };
                        if !dispatch_command(command, handle, &mut ui) {
                            return Ok(());
                        }
                    }
                }
                // While an overlay owns the screen (SPEC §7.5) it is modal: mouse
                // input is swallowed rather than moving the editor caret beneath the
                // prompt. (Routing clicks into overlays is an M7 concern.)
                Event::Mouse(_) if !overlays.is_empty() => {}
                Event::Mouse(mouse) => match mouse.kind {
                    // Left press or drag places/extends the caret at the pointer.
                    // A press is a plain click unless Shift is held (extend from the
                    // current anchor); a drag always extends, so a press-then-drag
                    // sweeps out a selection.
                    MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(snap) = &latest {
                            let is_press = matches!(mouse.kind, MouseEventKind::Down(_));
                            let extend = matches!(mouse.kind, MouseEventKind::Drag(_))
                                || mouse.modifiers.contains(KeyModifiers::SHIFT);
                            let offset = pointer_offset(snap, viewport, mouse.column, mouse.row);
                            // Alt+click adds a cursor without collapsing the set
                            // (SPEC §2.2 multi-cursor); a plain click/drag places or
                            // extends the single caret. Alt only adds on a fresh
                            // press, never mid-drag.
                            let action = if is_press && mouse.modifiers.contains(KeyModifiers::ALT)
                            {
                                Action::AddCursorAt { offset }
                            } else {
                                Action::PlaceCursor { offset, extend }
                            };
                            if handle.actions.send_blocking(action).is_err() {
                                return Ok(());
                            }
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
}

/// Dispatch one resolved frontend command (SPEC §7.5), from either a bound key or a
/// compositor layer committing a choice - one path for both. A core intent is
/// forwarded to the actor; a UI command opens an overlay or restyles the frontend.
/// Returns `false` when the app should exit (a quit, or the core's action channel
/// closed).
fn dispatch_command(command: Command, handle: &vortex_core::CoreHandle, ui: &mut Frontend) -> bool {
    match command {
        Command::Editor(action) => {
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
        Command::OpenThemePicker => ui
            .overlays
            .push(themepicker::open(&ui.config.theme, &ui.config.theme_name)),
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
fn pointer_offset(snapshot: &ViewSnapshot, viewport: ViewState, column: u16, row: u16) -> usize {
    let gutter_width = layout::gutter_width(layout::display_line_count(&snapshot.text));
    let last_body_row = viewport.page_height.saturating_sub(1);
    let body_row = (row.saturating_sub(1) as usize).min(last_body_row);
    layout::offset_at_cell(
        &snapshot.text,
        viewport.scroll,
        viewport.h_scroll,
        gutter_width,
        TAB_WIDTH,
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
struct PaintInputs {
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
    let cursor_display_col = layout::display_column(&line_text, cursor_byte_col, TAB_WIDTH);

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
    let line_width = layout::display_column(&line_text, line_text.len(), TAB_WIDTH);
    let max_h_scroll = (line_width + 1).saturating_sub(text_width);
    // When following the caret (keys, clicks, edits) each axis scrolls the minimum
    // to keep it visible; a wheel scroll turns follow off and paints the viewport's
    // own offset instead, so the view can move away from the caret. Both are clamped
    // to the content extent so a stale offset never paints blank rows/columns.
    let scroll = if follow {
        layout::scroll_to_show(cursor_line, viewport.scroll, text_height)
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

    paint_head_bar(frame, head_area, snapshot, theme.head_bar);
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
fn paint_head_bar(frame: &mut Frame, area: Rect, snapshot: &ViewSnapshot, style: Style) {
    let name = layout::buffer_display_name(snapshot.path.as_deref(), snapshot.modified);
    let (left, right) = layout::head_bar(&name, layout::display_line_count(&snapshot.text));
    let bar = layout::fit_bar(&left, &right, area.width as usize);
    frame.render_widget(Paragraph::new(bar).style(style), area);
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
    /// The cursor's line, so its gutter number can be emphasized.
    cursor_line: usize,
    /// The active theme, read straight through for the chrome styles this paints
    /// (gutter, selection, current line, secondary caret). `Theme` is `Copy`, so
    /// holding it beats copying each style field out by hand.
    theme: config::Theme,
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
    let lines = layout::visible_lines(text, body.scroll, height, TAB_WIDTH);

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
                        TAB_WIDTH,
                        s.start(),
                        s.end(),
                    )
                    .map(|range| (range, body.theme.selection))
                })
                .collect();
            // Syntax highlights (M4): each span clipped to this line's byte range,
            // mapped to display columns, painted as a foreground color over the
            // selection ground and under the diagnostic underline and carets.
            if !snapshot.decorations.is_empty() {
                for (span, kind) in snapshot
                    .decorations
                    .highlights_in(line_start..line_end_excl)
                {
                    if let Some(range) = layout::selection_columns(
                        raw,
                        line_start,
                        line_end_excl,
                        TAB_WIDTH,
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
                        TAB_WIDTH,
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
                    let col = layout::display_column(raw, head - line_start, TAB_WIDTH);
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
    );
    let bar = layout::fit_bar(&left, &right, area.width as usize);
    frame.render_widget(Paragraph::new(bar).style(status.style), area);
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

    /// Default per-frame paint inputs for tests: fresh view state, default theme,
    /// caret-follow on, and the given selection count. Tests needing a different
    /// viewport or follow flag override via struct update syntax.
    fn paint_inputs(selected: usize) -> PaintInputs {
        PaintInputs {
            viewport: ViewState::default(),
            theme: config::Theme::default(),
            follow: true,
            selected,
        }
    }

    /// Render `snapshot` into an in-memory `TestBackend` of `w`x`h` cells via the
    /// real [`paint`] path, and hand back the painted buffer for cell assertions.
    /// The selection count is derived from the snapshot, as the event loop does.
    fn render(snapshot: &ViewSnapshot, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let selected = layout::selected_grapheme_count(&snapshot.text, &snapshot.selections);
        terminal
            .draw(|frame| {
                paint(frame, snapshot, paint_inputs(selected));
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Parse a slice of string args (skipping argv[0]) the way `main` does.
    fn args(list: &[&str]) -> Args {
        parse_args(list.iter().map(OsString::from))
    }

    #[test]
    fn parse_args_no_args_opens_empty_buffer() {
        assert_eq!(args(&[]), Args::Open(None));
    }

    #[test]
    fn parse_args_positional_is_the_file() {
        assert_eq!(
            args(&["notes.txt"]),
            Args::Open(Some(PathBuf::from("notes.txt")))
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
            Args::Open(Some(PathBuf::from("--version")))
        );
    }

    #[test]
    fn parse_args_lone_dash_is_treated_as_a_path() {
        // A bare "-" is a conventional stdin placeholder, not an unknown flag;
        // keep it as a path rather than erroring.
        assert_eq!(args(&["-"]), Args::Open(Some(PathBuf::from("-"))));
    }

    #[test]
    fn parse_args_first_positional_wins() {
        assert_eq!(
            args(&["a.txt", "b.txt"]),
            Args::Open(Some(PathBuf::from("a.txt")))
        );
    }

    #[test]
    fn head_bar_shows_name_and_line_count_on_top_row() {
        let snap = snapshot_after(&[Action::Insert("a\nb\nc".into())]);
        let buf = render(&snap, 40, 10);
        let head = row_text(&buf, 0);
        assert!(head.contains(layout::NO_NAME), "head bar: {head:?}");
        assert!(head.contains("3 lines"), "head bar: {head:?}");
        // The whole row is painted with the head background (color, not a border).
        // Asserted against the theme, not a literal, so a retheme is not a test edit.
        let head_bg = config::Theme::default().head_bar.bg;
        assert_eq!(buf.cell((0, 0)).unwrap().bg, head_bg.unwrap());
        assert_eq!(buf.cell((39, 0)).unwrap().bg, head_bg.unwrap());
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
            let version = snap
                .expect("script must contain at least one action")
                .version;
            // Keep the highlighter's sync channel drained so the actor never blocks.
            while sync_rx.try_recv().is_ok() {}
            event_tx
                .send(SyntaxEvent::Highlights { version, spans })
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
        assert_eq!(pointer_offset(&snap, vp, 4, 2), 3);
        // A click on the head bar (screen row 0) clamps to the top line's start.
        assert_eq!(pointer_offset(&snap, vp, 4, 0), 0);
    }
}
