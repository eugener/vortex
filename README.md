# Vortex

A terminal text editor built as a **headless core plus a thin frontend**, so the terminal
is one possible frontend rather than the only one.

> **Status: early. Not usable as your daily editor yet.** It opens, edits, and saves files
> with multiple cursors, undo, syntax colors, and a working UI shell, and shows
> `rust-analyzer` diagnostics - but Rust is the only language with either a grammar or a
> language server, and there are no LSP features past diagnostics. See
> [Where it actually is](#where-it-actually-is) before trying it.

---

## The idea

Every editing decision lives in `vortex-core`, which has **no terminal dependencies at
all** - not as a style preference, but enforced by its `Cargo.toml`. The frontend sends
`Action`s in and reads `ViewSnapshot`s and `Delta`s out, over a channel:

```
        Action (intent)                     ViewSnapshot + Delta + Notification
  ┌──────────────────────┐            ┌──────────────────────────────────────┐
  │  vortex-tui          │ ─────────► │  vortex-core (single-owner actor)    │
  │  keys, viewport, UI  │ ◄───────── │  buffer, selections, history, files  │
  └──────────────────────┘            └──────────────────────────────────────┘
     ratatui + crossterm                      no crossterm, no ratatui
```

Three consequences fall out of that shape:

- **`Action` models intent, never keystrokes.** The core is sent `MoveCursorWordRight`,
  not `Ctrl+Right`. Key translation is the frontend's job, which is why the keymap is a
  data table rather than a `match` in the event loop.
- **The seam is already the RPC boundary.** `Action`, `Delta`, and `Notification` are
  `serde`-serializable from the start, so putting a socket between the two halves is
  adding a transport, not a redesign.
- **The core is testable without a terminal.** The primary test suite feeds it a script
  of `Action`s and asserts on what comes back - no PTY, no image snapshots.

The full architecture and the reasoning behind each decision (including the ones
deliberately *not* taken) is in **[`docs/SPEC.md`](docs/SPEC.md)**.

## Where it actually is

Milestones from [SPEC §14](docs/SPEC.md). The build order is risk-first, so some later
milestones landed before earlier ones - that is a real gap, not a rounding error.

| | Milestone | State |
|---|---|---|
| M0 | Workspace + seam skeleton | done |
| M1 | Edit + render pipeline | done |
| M2 | Async runtime + LSP | done - `rust-analyzer` diagnostics for Rust files, underlined by span |
| M3 | Anchors, undo tree, multi-cursor | done |
| M4 | Syntax highlighting (tree-sitter) | done - grammars are `dlopen`ed at runtime, never linked in |
| M5 | File handling (encoding, EOL, conflicts) | done - encoding and line endings survive a round trip, external changes are noticed, config is a file |
| M6 | UI shell: compositor, toasts | done |
| M7 | Pickers, palette, themes, search | done - file/theme/buffer/global-search pickers with a preview pane, the palette, multi-buffer, find/replace in the buffer and across the project |
| M8 | Chrome and polish | not started |

What works today: open/save a file, edit with multiple cursors, undo/redo with coalescing,
mouse selection everywhere (including the overlays and the status bar), the system
clipboard (including over SSH via OSC 52), several buffers with a tab strip, tree-sitter
syntax colors on Rust, a fuzzy file picker and a buffer picker (both with a preview pane),
a command palette, switchable themes, regex find-and-replace in the buffer (matches
highlighted as you type, capture groups in the replacement, and a query-replace walk),
project-wide regex search that jumps to the match, a config file for keys and settings, files in their own encoding and line endings (saved
back as they were found), a warning when a file changes underneath you, and live
`rust-analyzer` diagnostics (underlined by span, with a colored gutter mark, if
`rust-analyzer` is on your PATH).

What does not: syntax colors or a language server for **any language but Rust** - a
grammar is a separate crate the editor loads at runtime, and only `grammar-rust` is
written; LSP features past diagnostics (no completion, hover, or goto); and most of the
M8 chrome (git diff signs, indent guides, a scrollbar, sticky context, rulers).

There is no which-key popup and no leader-key sequences: bindings are single chords, and
"what can I do here" is answered by the command palette, which lists every command by name
with its shortcut beside it. [SPEC §11](docs/SPEC.md) records the trigger for revisiting
that.

## Build and run

Rust 2024 edition (built against 1.97).

```sh
git clone https://github.com/eugener/vortex && cd vortex
cargo build --release
./target/release/vortex path/to/file.txt
```

`vortex --help` prints the key list. `vortex` with no argument opens an empty buffer.

## Keys

| | |
|---|---|
| `Ctrl+S` / `Ctrl+Q` | Save / quit |
| `Ctrl+Shift+S` | Save as (Kitty terminals only - elsewhere it folds to an ordinary save) |
| `Ctrl+O` | Open a file (fuzzy picker over the working directory, previewing the highlighted one) |
| `Ctrl+F` | Find in this buffer (regex; matches highlight and the view follows as you type, `Enter` goes there) |
| `F3` / `Shift+F3` | Next / previous match (`Ctrl+G` also finds the next) |
| `Ctrl+H` | Find and replace (`Tab` between the fields; then `y`/`n`/`a`/`q` per match) |
| `Ctrl+Shift+F` or `Alt+F` | Search the project (regex, gitignore-aware; Enter jumps to the match) |
| `Ctrl+Shift+L` | Put a cursor on every match of the last search |
| `Ctrl+P` | Command palette |
| `Ctrl+T` | Theme picker (previews as you move, `Esc` restores) |
| `Ctrl+B` / `Ctrl+W` | Buffer picker / close the current buffer |
| `Ctrl+PageUp` / `Ctrl+PageDown` | Previous / next buffer |
| `Ctrl+Alt+Up/Down` | Add a cursor above/below |
| `Alt+Click` | Add a cursor at the pointer |
| `Esc` | Collapse back to one cursor, and clear the search highlights |
| Arrows, `Home`/`End`, `PageUp`/`PageDown` | Move; hold `Shift` to select |

Undo/redo and clipboard follow each OS: `Cmd+Z`/`Cmd+Y` and `Cmd+C`/`X`/`V` on macOS,
`Ctrl+` the same keys elsewhere. On macOS `Ctrl+C` stays quit, since copy is `Cmd+C` there.

The `Ctrl+Alt` and `Ctrl+Shift` chords need a terminal that speaks the [Kitty keyboard
protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) (Kitty, Ghostty, WezTerm,
foot, recent Alacritty). It is negotiated at startup; where it is missing those chords
simply never fire rather than misfiring. Project search is bound to `Alt+F` as well for
that reason, and everything else in this table is reachable from the palette.

## Mouse

| | |
|---|---|
| Click / drag | Place the caret, or sweep out a selection |
| Double / triple click | Select the word / the whole line |
| Click the gutter | Select that line |
| `Alt+Click` | Add a cursor |
| `Shift+Click` | Extend the selection |
| Wheel | Scroll the view (or move the highlight, in a picker) |
| Click a tab | Switch to that buffer |
| Click a picker row | Run it - one click, not two |
| Click outside an overlay | Dismiss it |
| Click a toast | Dismiss it early |
| Click the encoding or `LF`/`CRLF` in the status bar | Change it |

## Themes

Four ship with the editor, compiled into the binary:

| Theme | |
|---|---|
| **undertow** | the default. Conventional dark; depth carried by blue, one accent reserved for state |
| **instrument** | achromatic, with a single signal red for *where you are* and *what is wrong*. Loses nothing on a monochrome terminal or to color blindness |
| **daylight** | light, for a lit room. The selection is a highlighter stroke that keeps the text's own color instead of washing it out |
| **phosphor** | the amber CRT taken literally: one hue, five intensities, escalation by inverse video |

Press `Ctrl+T` to switch. The picker **previews** - moving the highlight applies the theme
immediately, `Enter` keeps it, `Esc` restores the one you started in. A pick lasts the
session; `theme = "phosphor"` in the config file below makes it the one you start in.

### Writing your own

Drop a `.toml` file in `$XDG_CONFIG_HOME/vortex/themes/` (or `~/.config/vortex/themes/`).
The file's name is the theme's name, and a file that shares a name with a built-in
**shadows** it - so to edit a shipped theme, copy it there and change it.

```toml
# ~/.config/vortex/themes/mine.toml
text             = { fg = "#c9ced2", bg = "#141719" }   # the editor's own ground
head_bar         = { fg = "#eef2f4", bold = true }
status_bar       = { fg = "#78838a" }
gutter           = { fg = "#4e575e" }
gutter_current   = { fg = "#eef2f4", bold = true }
selection        = { fg = "#eef2f4", bg = "#2f383e" }
current_line     = { bg = "#1b1f22" }
secondary_cursor = { fg = "#141719", bg = "#e04b3c" }
toast_info       = { fg = "#c9ced2", bg = "#242a2e" }
toast_error      = { fg = "#141719", bg = "#e04b3c", bold = true }
palette          = { fg = "#c9ced2", bg = "#1b1f22" }
palette_selected = { fg = "#eef2f4", bg = "#2f383e", bold = true }
```

- **Every key is optional** and inherits the default, so a theme that only recolors the
  selection is two lines. A key that *is* present replaces its default outright rather
  than merging into it.
- **Colors are `#rrggbb` only.** Named ANSI colors get remapped by your terminal profile,
  so a theme built from them cannot promise the contrast it was designed with.
- **Attributes:** `bold`, `dim`, `italic`, `underlined`, `reversed`.
- **A typo is an error, not a shrug.** An unknown key fails the load and says so, rather
  than leaving you wondering why nothing changed.

The shipped themes live in [`crates/tui/themes/`](crates/tui/themes) and are the best
worked examples.

## Configuration

`$XDG_CONFIG_HOME/vortex/config.toml`, else `~/.config/vortex/config.toml`. Every key is
optional, so the file can be one line; `--config PATH` reads a different one.

```toml
theme         = "phosphor"   # any built-in or one of your own
tab_width     = 4            # display width of a tab stop
final_newline = true         # add a trailing newline on save if the file lacks one

[keys]                       # layered over the built-in bindings, not replacing them
"ctrl+w"      = "close_buffer"
"ctrl+alt+up" = "add_cursor_above"
```

Chords are `mod+mod+key` with `ctrl`/`shift`/`alt` in any order; command names are the
same identifiers the palette lists. Rebinding one chord leaves every other binding alone.

Like a theme file, **an unknown key is an error rather than a shrug** - but a config that
does not load never stops the editor coming up. It starts on the defaults and says what
was wrong in a toast.

## Layout

```
crates/
  core/     vortex-core - buffer, selections, anchors, history, file I/O.
            No crossterm. No ratatui. The compiler enforces it.
  tui/      vortex-tui  - the `vortex` binary: keymap, viewport math, overlay
            compositor, pickers, themes. Thin by design.
  tui/themes/     the built-in theme files
  grammar-rust/   a tree-sitter grammar as a cdylib, `dlopen`ed at runtime.
            Not a dependency of the editor - that is what makes a language addable.
docs/SPEC.md   the architecture and its decision record
```

Some invariants worth knowing before changing anything (the full list is in
[`CLAUDE.md`](CLAUDE.md)):

- The core/frontend boundary is **message passing**, never a method call across the seam.
- Cursor state is always a `SelectionSet`, never a single cursor. Motions and edits map
  over the set.
- Anything that outlives one edit (LSP positions, file watches) uses **anchors**, never
  raw byte offsets.
- The buffer sits behind a `Buffer` trait; `crop::Rope` never appears in the core's public
  API, so the backend stays swappable.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
cargo llvm-cov --package vortex-core --fail-under-file-lines 90 --summary-only
cargo llvm-cov --package vortex-tui  --fail-under-file-lines 60 --summary-only
```

All five must pass, with no warnings, before a change is done. Coverage is **per file**
and ratcheted - a floor of 90% for the core (currently 99.4%) and 60% for the frontend
(currently 89.9%), because the frontend has a genuinely untestable I/O shell and the core
has no excuse. Line coverage is a floor, not the goal; the real correctness bar is the
property tests (`proptest`) that generate random `Action` sequences and assert the model's
invariants hold.

`cargo llvm-cov` needs `cargo install cargo-llvm-cov` and
`rustup component add llvm-tools-preview`.

## License

MIT. See [`LICENSE`](LICENSE).
