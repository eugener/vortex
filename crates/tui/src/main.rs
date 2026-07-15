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

mod keymap;
mod layout;

use std::io::{self, Stdout};
use std::time::Duration;

use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, supports_keyboard_enhancement,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Position;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};

use vortex_core::{Action, Core, ViewSnapshot};

/// Default tab stop width for display-column layout (SPEC §4). Config in M5.
const TAB_WIDTH: usize = 4;
/// How long the input poll blocks before we tick the render loop anyway, so a
/// snapshot that arrives without a keystroke (e.g. a background restyle in M4)
/// still gets painted promptly.
const POLL: Duration = Duration::from_millis(16);

fn main() -> io::Result<()> {
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

    // Terminal setup. On any error we still attempt teardown so we never leave the
    // user's terminal in raw mode (the Drop impl is the backstop).
    let mut term = TerminalGuard::enter()?;
    let result = event_loop(&handle, &mut term.terminal);
    term.leave();

    // Dropping the handle closes the action channel, so the core loop ends; join
    // it so the process does not exit while the actor is mid-shutdown.
    drop(handle);
    let _ = core_thread.join();
    result
}

/// The render + input loop, run synchronously on the main thread. Returns when the
/// user quits or a channel closes. Uses blocking channel ops (`send_blocking`)
/// against the core running on its own thread; painting is driven by whichever
/// comes first each tick - an input event or the poll timeout, so a snapshot that
/// arrives without a keystroke (e.g. a background restyle in M4) still paints.
fn event_loop(
    handle: &vortex_core::CoreHandle,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
) -> io::Result<()> {
    // Prime the view: ask the core for an initial snapshot to paint. Surface a
    // failed prime (core thread never started) rather than sitting on a blank
    // screen forever.
    if handle
        .actions
        .send_blocking(Action::RequestSnapshot)
        .is_err()
    {
        return Ok(());
    }
    let mut latest: Option<ViewSnapshot> = None;
    let mut scroll: usize = 0;
    // Repaint only when something changed - a new snapshot, a resize, or the
    // first frame. Redrawing every idle poll tick is wasted work (ratatui
    // cell-diffs, so it emits nothing, but it still rebuilds the frame ~60x/sec).
    let mut needs_redraw = true;

    loop {
        // Take the newest snapshot if the core published one (latest-wins cell).
        if let Some(snap) = handle.snapshots.try_recv() {
            latest = Some(snap);
            needs_redraw = true;
        }
        // Drain the delta channel so its bounded buffer never fills; a local
        // terminal frontend paints from the snapshot, using deltas only as a
        // future partial-repaint hint (SPEC §5, §6).
        while handle.deltas.try_recv().is_ok() {}

        if let Some(snap) = &latest
            && needs_redraw
        {
            scroll = draw(terminal, snap, scroll)?;
            needs_redraw = false;
        }

        // Wait for input, but no longer than POLL so a snapshot arriving without a
        // keystroke still gets painted on the next tick.
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(action) = keymap::action_for_key(key) {
                        let quit = action == Action::Quit;
                        // If the core is gone, exit cleanly.
                        if handle.actions.send_blocking(action).is_err() || quit {
                            return Ok(());
                        }
                    }
                }
                // Repaint against the new terminal size.
                Event::Resize(_, _) => needs_redraw = true,
                _ => {}
            }
        }
    }
}

/// Paint one frame from `snapshot`, wrapped in synchronized output (anti-tearing,
/// SPEC §7). Returns the (possibly adjusted) vertical scroll offset so the primary
/// cursor stays visible. All layout math is delegated to the tested [`layout`]
/// module.
fn draw(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    snapshot: &ViewSnapshot,
    scroll: usize,
) -> io::Result<usize> {
    let height = terminal.size()?.height as usize;

    // Primary cursor position in line/grapheme-column space, from the snapshot.
    // Follow the primary selection (SPEC §2.2), not a positional guess.
    let head = snapshot
        .selections
        .get(snapshot.primary)
        .map(|s| s.head)
        .unwrap_or(0);
    let (cursor_line, cursor_byte_col, line_text) = layout::cursor_line_col(&snapshot.text, head);
    let new_scroll = layout::scroll_to_show(cursor_line, scroll, height);
    let cursor_display_col = layout::display_column(&line_text, cursor_byte_col, TAB_WIDTH);

    // Compute the visible-line slice outside the draw closure (tested in layout).
    let lines = layout::visible_lines(&snapshot.text, new_scroll, height, TAB_WIDTH);

    let mut out = io::stdout();
    queue!(out, BeginSynchronizedUpdate)?;

    terminal.draw(|frame| {
        let area = frame.area();
        let visible: Vec<Line> = lines
            .into_iter()
            .map(|l| Line::from(Span::raw(l)))
            .collect();
        frame.render_widget(Paragraph::new(visible), area);

        // Place the terminal cursor at the primary caret (screen-relative).
        // `saturating_sub` guards the case where scroll sits below the cursor
        // line (e.g. a zero-height resize left `new_scroll` stale) - never
        // underflow into a wild u16 row (SPEC §8: a resize must not crash).
        let row = cursor_line.saturating_sub(new_scroll) as u16;
        frame.set_cursor_position(Position::new(cursor_display_col as u16, row));
    })?;

    execute!(out, EndSynchronizedUpdate)?;
    Ok(new_scroll)
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
        execute!(out, ratatui::crossterm::terminal::EnterAlternateScreen)?;

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
        let _ = execute!(out, ratatui::crossterm::terminal::LeaveAlternateScreen);
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}
