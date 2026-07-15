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
use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, supports_keyboard_enhancement,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use vortex_core::{Action, Core, ViewSnapshot};

/// Default tab stop width for display-column layout (SPEC §4). Config in M5.
const TAB_WIDTH: usize = 4;

/// Chrome palette. Both bars paint a filled background row (the user asked for
/// color, not divider lines); the gutter and current-line number are dimmed/bold
/// to sit behind the text without competing with it. Kept as `const` because
/// `Style::new()` and its setters are `const` in ratatui 0.30.
const HEAD_STYLE: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Cyan)
    .add_modifier(Modifier::BOLD);
const STATUS_STYLE: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
/// Gutter line numbers: dim so they recede behind the text.
const GUTTER_STYLE: Style = Style::new().fg(Color::DarkGray);
/// The cursor's line number: bold + brighter so the active row stands out.
const GUTTER_CURRENT_STYLE: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
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

    // First positional argument is the file to open (`vortex path`). A missing
    // file is not an error - the core opens an empty buffer bound to it, created
    // on the first save (SPEC §10). Extra args are ignored until multi-buffer.
    let path = std::env::args_os().nth(1).map(PathBuf::from);

    // Terminal setup. On any error we still attempt teardown so we never leave the
    // user's terminal in raw mode (the Drop impl is the backstop).
    let mut term = TerminalGuard::enter()?;
    let result = event_loop(&handle, &mut term.terminal, path);
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
    path: Option<PathBuf>,
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
    let mut scroll: usize = 0;
    // The latest file-lifecycle message (open/save result), shown transiently in
    // the status bar until the next edit or motion snapshot clears it. A failed
    // save must be visible, not silent (SPEC §8).
    let mut message: Option<String> = None;
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
        // Drain notifications: the bounded channel must not fill (every save emits
        // one, SPEC §6), and a file open/save result is surfaced in the status bar
        // (SPEC §8). Keep the latest message worth showing.
        while let Ok(note) = handle.notifications.try_recv() {
            if let Some(m) = layout::notification_message(&note) {
                message = Some(m);
                needs_redraw = true;
            }
        }

        if let Some(snap) = &latest
            && needs_redraw
        {
            scroll = draw(terminal, snap, scroll, message.as_deref())?;
            needs_redraw = false;
        }

        // Wait for input, but no longer than POLL so a snapshot arriving without a
        // keystroke still gets painted on the next tick.
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(action) = keymap::action_for_key(key) {
                        let quit = action == Action::Quit;
                        // A new user action clears the transient file message so it
                        // does not linger while the user keeps typing; Save keeps it
                        // (its own result replaces it once the notification lands).
                        if !matches!(action, Action::Save) && message.take().is_some() {
                            needs_redraw = true;
                        }
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
/// cursor stays visible. The frame composition itself lives in [`paint`] so it can
/// be rendered against a `TestBackend` and asserted cell-by-cell (SPEC §13).
fn draw(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    snapshot: &ViewSnapshot,
    scroll: usize,
    message: Option<&str>,
) -> io::Result<usize> {
    let mut new_scroll = scroll;
    let mut out = io::stdout();
    queue!(out, BeginSynchronizedUpdate)?;
    terminal.draw(|frame| new_scroll = paint(frame, snapshot, scroll, message))?;
    execute!(out, EndSynchronizedUpdate)?;
    Ok(new_scroll)
}

/// Compose the whole frame: head bar, gutter + text, status bar, and the cursor.
/// Backend-generic (takes a `&mut Frame`) so a `TestBackend` render can assert on
/// the painted cells (SPEC §13). Returns the scroll offset it settled on so the
/// caller can carry it forward. All measurement is delegated to the tested
/// [`layout`] helpers; this function only positions widgets.
fn paint(
    frame: &mut Frame,
    snapshot: &ViewSnapshot,
    scroll: usize,
    message: Option<&str>,
) -> usize {
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

    let text_height = body_area.height as usize;
    let new_scroll = layout::scroll_to_show(cursor_line, scroll, text_height);
    let cursor_display_col = layout::display_column(&line_text, cursor_byte_col, TAB_WIDTH);

    paint_head_bar(frame, head_area, snapshot);
    let gutter_width = paint_body(frame, body_area, snapshot, new_scroll, cursor_line);
    paint_status_bar(
        frame,
        status_area,
        snapshot,
        cursor_line,
        &line_text,
        cursor_byte_col,
        message,
    );

    // Place the terminal cursor at the primary caret, offset by the gutter and the
    // head row. `saturating_sub` guards a stale scroll after a zero-height resize -
    // never underflow into a wild u16 row (SPEC §8: a resize must not crash).
    let row = body_area.y + cursor_line.saturating_sub(new_scroll) as u16;
    let col = body_area.x + (gutter_width + cursor_display_col) as u16;
    frame.set_cursor_position(Position::new(col, row));
    new_scroll
}

/// Paint the top head bar (buffer name left, line count right) as one filled row.
/// The name is the bound file's name plus a modified marker (SPEC §8, §10), read
/// straight from the snapshot so painting needs no core round-trip (SPEC §5).
fn paint_head_bar(frame: &mut Frame, area: Rect, snapshot: &ViewSnapshot) {
    let name = layout::buffer_display_name(snapshot.path.as_deref(), snapshot.modified);
    let (left, right) = layout::head_bar(&name, layout::display_line_count(&snapshot.text));
    let bar = layout::fit_bar(&left, &right, area.width as usize);
    frame.render_widget(Paragraph::new(bar).style(HEAD_STYLE), area);
}

/// Paint the text body with a line-number gutter. Returns the gutter width (cells)
/// so the caller can offset the cursor. Each visible row is a gutter span (dim, or
/// bold for the cursor's line) followed by the tab-expanded line text.
fn paint_body(
    frame: &mut Frame,
    area: Rect,
    snapshot: &ViewSnapshot,
    scroll: usize,
    cursor_line: usize,
) -> usize {
    let text = &snapshot.text;
    let gutter_width = layout::gutter_width(layout::display_line_count(text));
    let height = area.height as usize;
    let lines = layout::visible_lines(text, scroll, height, TAB_WIDTH);

    let rows: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .map(|(row, content)| {
            let line_index = scroll + row;
            let gutter_style = if line_index == cursor_line {
                GUTTER_CURRENT_STYLE
            } else {
                GUTTER_STYLE
            };
            Line::from(vec![
                Span::styled(layout::gutter_label(line_index, gutter_width), gutter_style),
                Span::raw(content),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(rows), area);
    gutter_width
}

/// Paint the bottom status bar. Normally shows cursor position (left) and buffer
/// metrics (right); when a transient file `message` is present it replaces the
/// cursor position so an open/save result - especially a failure - is visible
/// (SPEC §8).
fn paint_status_bar(
    frame: &mut Frame,
    area: Rect,
    snapshot: &ViewSnapshot,
    cursor_line: usize,
    line_text: &str,
    cursor_byte_col: usize,
    message: Option<&str>,
) {
    let col = layout::grapheme_column(line_text, cursor_byte_col);
    let (left, right) = layout::status_bar(
        cursor_line + 1,
        col,
        snapshot.text.byte_len(),
        snapshot.version,
        message,
    );
    let bar = layout::fit_bar(&left, &right, area.width as usize);
    frame.render_widget(Paragraph::new(bar).style(STATUS_STYLE), area);
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// A temp directory removed on drop, so a test that opens a real file cleans
    /// up even if an assertion panics first (a bare trailing `remove_dir_all`
    /// would leak the dir on failure). Name mixes pid + a counter to avoid
    /// collisions across parallel tests.
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("vortex-tui-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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

    /// Render `snapshot` into an in-memory `TestBackend` of `w`x`h` cells via the
    /// real [`paint`] path, and hand back the painted buffer for cell assertions.
    fn render(snapshot: &ViewSnapshot, w: u16, h: u16) -> ratatui::buffer::Buffer {
        render_with_message(snapshot, w, h, None)
    }

    /// As [`render`], but with a transient status-bar `message` (a file open/save
    /// result) so its placement can be asserted.
    fn render_with_message(
        snapshot: &ViewSnapshot,
        w: u16,
        h: u16,
        message: Option<&str>,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| {
                paint(frame, snapshot, 0, message);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The concatenated symbols of row `y`, for substring assertions on a bar.
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area().width)
            .map(|x| buf.cell((x, y)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn head_bar_shows_name_and_line_count_on_top_row() {
        let snap = snapshot_after(&[Action::Insert("a\nb\nc".into())]);
        let buf = render(&snap, 40, 10);
        let head = row_text(&buf, 0);
        assert!(head.contains(layout::NO_NAME), "head bar: {head:?}");
        assert!(head.contains("3 lines"), "head bar: {head:?}");
        // The whole row is painted with the head background (color, not a border).
        assert_eq!(buf.cell((0, 0)).unwrap().bg, Color::Cyan);
        assert_eq!(buf.cell((39, 0)).unwrap().bg, Color::Cyan);
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
    fn status_bar_shows_file_message_in_place_of_position() {
        // With a transient file message the bottom bar shows it instead of the
        // cursor position (SPEC §8: a save result must be visible).
        let snap = snapshot_after(&[Action::Insert("hi".into())]);
        let buf = render_with_message(&snap, 40, 10, Some("Saved out.txt"));
        let status = row_text(&buf, 9);
        assert!(status.contains("Saved out.txt"), "status: {status:?}");
        assert!(
            !status.contains("Ln 1"),
            "position should be replaced: {status:?}"
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
        assert_eq!(buf.cell((0, 9)).unwrap().bg, Color::Cyan);
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
    fn cursor_line_gutter_is_emphasized() {
        // Cursor ends on line 2; its gutter number is bold+white, the other dim.
        let snap = snapshot_after(&[Action::Insert("x\ny".into())]);
        let buf = render(&snap, 40, 10);
        // The '1' digit sits in column 2 of the 4-wide gutter ("  1 ").
        let inactive = buf.cell((2, 1)).unwrap();
        let active = buf.cell((2, 2)).unwrap();
        assert_eq!(inactive.fg, Color::DarkGray);
        assert_eq!(active.fg, Color::White);
        assert!(active.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cursor_sits_after_the_gutter() {
        // Fresh empty buffer: cursor at Ln 1 Col 1, painted just right of the
        // 4-cell gutter on the first body row (row 1).
        let snap = snapshot_after(&[Action::RequestSnapshot]);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                paint(frame, &snap, 0, None);
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
}
