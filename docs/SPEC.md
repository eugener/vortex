# Vortex - Editor Spec

A terminal-based text editor built as a **headless core + thin frontend**, so the
terminal is one of several possible frontends. Written in Rust.

Status: draft. Scope today is **terminal-only**, but every boundary is shaped so a
GUI / web / remote frontend can attach later without rewriting the core.

**Reading order:** §1 (boundary) → §2 (data model) → §4 (coordinates) → §5 (render
model) are the load-bearing decisions. Everything else supports them.

---

## 0. Non-goals (for the current scope)

Stating these bounds the design; each may be revisited, but none is built now.

- **No collaboration / multi-user editing.** No CRDT replica model (see §11 for the
  seam that keeps it possible).
- **No out-of-process frontend yet.** The seam is in-process message-passing (§1).
- **No plugin runtime yet.** The engine is an open decision (§12.1); the boundary is
  designed so it can be added without core changes.
- **No GUI/web frontend yet**, only terminal. The core stays view-agnostic so they can
  attach later.
- **Large files: Tiers 1-2 supported, Tier 3 (bigger-than-RAM editing) deferred.** Files
  up to a few hundred MB edit fully; multi-GB files degrade gracefully (§10.4). We do not
  build a paged/mmap buffer for editing files larger than RAM - that collides with the §5
  render model and is kept as a swap-ready seam (§11), not a v1 feature.

---

## 1. Guiding principle: the protocol-shaped boundary

The hard part of "one backend, many frontends" is not the TUI library. It is deciding
**what the core owns vs. what the frontend owns**, and **how they talk**.

Reference points that bracket the design space:

- **Neovim** - headless C core, UIs attach over MessagePack-RPC. Core owns the screen
  grid; UIs are thin. Proved "plugins are just RPC clients sending the same messages the
  UI sends."
- **Xi editor** (archived) - Rust core + JSON-RPC frontends, goal of "frontend has zero
  editing logic." Retrospective lesson: that purity was *too* strict - async round-trips
  for styling/scrolling made the UI feel laggy. **We avoid this concretely in §5.**

**Our line:** core owns buffer state, undo, LSP, syntax, and *authoritative* styling.
Frontend owns viewport (which lines are visible), scrolling, and cursor rendering - all
read locally from a snapshot, never via a round-trip.

### The seam is a message channel, not a function API

Both sides are Rust and there is only a terminal today, so running the core
out-of-process now would be pure overhead for zero benefit. But we still build the
boundary, in-process, as message-passing:

```
Frontend  ── Action ─────────▶  [ Core: single-owner actor task ]
Frontend  ◀─ Delta ───────────     owns SelectionSet, buffers,
Frontend  ◀─ ViewSnapshot ────     undo tree, syntax trees, decorations
Frontend  ◀─ Notification ────     (select! over inbound channels)
LSP client ─ Response ───────▶
FS watcher ── FsEvent ───────▶
```

Core→frontend streams, chosen deliberately (see §6 for channel types):
- **`Action`** (frontend → core): intent to change state.
- **`Delta`** (core → frontend): the authoritative "what changed" stream (§5). The wire
  protocol for remote frontends; local frontends may ignore it in favor of the snapshot.
- **`ViewSnapshot`** (core → frontend): a *derived*, latest-wins, `Arc`-shared render state
  for local frontends (§5).
- **`Notification`** (core → frontend): discrete events - errors, status, prompts (§8).

This is the shape of JSON-RPC minus the wire. The day a non-Rust or remote frontend
exists, we insert a `serde` + socket layer **at that exact seam**. `Action`, `Notification`,
and `Delta` are small value messages that translate to the wire essentially for free (add
`#[derive(Serialize, Deserialize)]`, channel becomes a socket). `ViewSnapshot` carries the
whole buffer and does not serialize cheaply - but it never needs to: it is a local-only
convenience, and the remote transport ships the `Delta` stream the core already produces
(§5). Starting with direct method calls (`editor.insert_char(...)`) would hardcode
synchronous assumptions and make extracting the boundary a rewrite; keeping the seam as
messages means remote support is transport wiring in `proto/`, not core changes.

The same boundary is **also the plugin API** (Neovim's proof) and makes the core
**trivially testable** (§13): feed a script of `Action`s, assert on emitted
`ViewSnapshot`/`Notification`s - no terminal, no PTY. One mechanism buys alternative
frontends, remote editing, extensibility, and testing.

---

## 2. Core data-model decisions

The stack is downstream of these. Pick them wrong and no library helps.

### 2.1 Buffer + anchors (the correctness lynchpin)

**Decision: rope (`crop`) + a thin anchor layer we own.**

Anchors are positions that **survive edits** - insert text before an anchor and it moves
with the text. Not a collaboration feature; it is what makes *any* async editing correct
on a single machine. The moment an LSP diagnostic ("error at byte 1234") or a file-watcher
event races the user's keystrokes, raw byte offsets point at the wrong place and anchors
do not. Diagnostics, marks, folds, multi-cursor, and search results all attach to anchors.

Tiers considered:
- Plain rope + byte offsets - breaks under any async buffer access. Rejected.
- **Rope + our own anchor layer (Helix model) - chosen.**
- CRDT buffer (Zed model) - collapses undo/anchors/multi-cursor/collab into one
  mechanism, but costs memory-per-character and complexity, and its payoff (optimistic
  local apply + conflict-free reconcile) only materializes across a *network*.
  Terminal-in-process has no network. **Deferred** (§11).

**Anchor semantics (must be specified, not left implicit):**
- An `Anchor` is an opaque handle resolvable to a current byte offset against a specific
  buffer's version. Versions are **per-buffer** (§5), so an edit in one buffer never
  invalidates another buffer's anchors.
- **Bias:** each anchor has a `Bias` (`Before` / `After`) deciding which side it sticks to
  when an insertion happens *exactly at* its position. A selection's start is typically
  `Before`-biased, its end `After`-biased, so typing inside a selection grows it. This
  detail is the difference between correct and subtly-wrong selection behavior.
- **Deletion:** if the anchored position is inside deleted text, the anchor collapses to
  the deletion boundary (deterministic, documented).
- Implementation baseline: maintain anchors by transforming them through each `Edit`
  (offset shift). The API is shaped so a future CRDT backing (stable per-anchor IDs) is a
  drop-in without changing call sites.
- **The buffer sits behind a `Buffer` trait; `crop::Rope` never leaks into the core's
  public surface.** Selections, undo, syntax, and actions talk to the abstraction, not to
  `crop` directly. This keeps two future backends swap-ready without touching call sites:
  a CRDT (above) and a Tier-3 paged/mmap buffer for bigger-than-RAM files (§10.4, §11).

### 2.2 Selection set, not a single cursor

**Decision: cursor state is a `SelectionSet` from commit one.**

Kakoune/Helix's best idea: a cursor is a zero-width selection, and the editor always holds
a *set*. Every motion/edit maps over the set. Multi-cursor, block selection, and
"select-all-matches then edit" become the default model instead of bolted-on features.
Retrofitting this onto a single-cursor core is one of the most painful editor refactors.

- One selection is the **primary** (drives viewport-follow requests, prompts).
- Overlapping selections **merge** after every motion/edit (documented invariant: the set
  is always disjoint and sorted).

### 2.3 Core concurrency: single-owner actor

**Decision: one task owns all editor state; everything else talks to it by message.**

Not `Arc<RwLock<Editor>>` shared across threads - that is the road to
held-lock-across-`.await` deadlocks. Instead:

- One task owns `SelectionSet`, buffers, undo tree, syntax trees, decorations. Edits mutate
  directly: single owner, zero locking, no data races.
- LSP client and FS watcher are async tasks that **send messages in**.
- Heavy tree-sitter reparses run on a **cheap `crop` snapshot** off the critical path
  (via the `blocking` crate's `unblock` / a dedicated thread pool), then send results back
  in (§5).
- The core `select!`s over all inbound channels.

### 2.4 Undo tree + coalescing

**Decision: undo tree, with time/boundary-based coalescing.**

- **Tree, not stack:** undo-then-type on a stack destroys the redo branch and loses work.
  A tree keeps every branch reachable (Vim/Neovim do this). Each history node references
  anchors, composing with §2.1.
- **Coalescing:** consecutive single-character inserts are grouped into one undo unit,
  broken by (a) a time gap, (b) a non-adjacent edit, (c) a newline, or (d) a
  cursor/selection change. Without this, undo reverts one character at a time - unusable.
  - Rule (d) is enforced **structurally**, not by each action announcing it: every edit
    is recorded with the selection set it started from, and coalescing is refused when
    that differs from the previous edit's post-state. No action arm can forget to break
    the run, which is what keeps the rule true as the selection vocabulary grows. The
    corollary is that a motion returning to the *exact* same selection set does not
    break the run - indistinguishable from never having moved, and accepted as such.
- **One `Action` is one undo unit, even across multiple cursors.** A single keystroke
  applied over an N-cursor `SelectionSet` is N disjoint text edits but **one** undo entry -
  the break rules above are about *separate actions over time*, never about one action
  fanned across the selection set. (Otherwise multi-cursor typing would create an undo unit
  per keystroke and undo would be unusable in exactly the mode where it matters most.)
- Reference shape: Helix's `history` module.

---

## 3. Stack (2026)

| Layer | Choice | Notes |
|---|---|---|
| Workspace | Cargo workspace: `crates/core` (no terminal deps), `crates/tui`, later `crates/proto` | Boundary enforced at compile time |
| Text storage | **`crop`** | Rope; `Arc`-shared, `Send + Sync`, clone is "extremely cheap" (verified) |
| Anchors | our own thin layer over `crop` | swappable to CRDT later |
| Grapheme/width | **`unicode-segmentation`** + **`unicode-width`** | correct cursor movement + display columns (§4) |
| Syntax | **`tree-sitter`** + grammar crates | incremental reparse, error-tolerant, no server |
| LSP | **`async-lsp`** (tower-based, runtime-agnostic) | client: diagnostics, completion, goto |
| Async runtime | **`smol` / `async-executor`** | lean binary + build; LSP-compatible via async-lsp |
| Channels | **`async-channel`** (bounded) + latest-wins snapshot cell | see §6 |
| Terminal render | **`ratatui` + `crossterm`** | immediate-mode cell-diffing; we own the loop |
| Frame atomicity | crossterm `BeginSynchronizedUpdate` / `EndSynchronizedUpdate` | anti-tearing (§7) |
| Fuzzy match | **`nucleo-matcher`** | palette/picker ranking (Helix's matcher); on-thread for small lists (§7.5) |
| Search patterns | **`regex`** | both searches (M7). Reached `vortex-tui` first, since cross-file search is filesystem work and so frontend-owned, and joined `vortex-core` with the in-buffer one (§11) - **one engine, two crates**, so the two searches cannot diverge on what a pattern means, and the frontend's live preview compiles through the core's own `search::compile` rather than a second copy |
| Project walk | **`ignore`** | gitignore-aware walker (ripgrep's). Global search respects a project's own `.gitignore` rather than a hardcoded skip list |
| Config | **`toml` + `serde`** | Helix-style; in `vortex-tui` now, carrying theme files (§10.5) |
| Encoding | `encoding_rs` | detect on load; edit as UTF-8 internally (§10.1) |
| File watch | `notify` | external-change detection (§10.2) |
| Error types | **`thiserror`** (libs) | typed errors across the seam (§8) |
| Extensibility | **OPEN** (§12.1) | rides the same message boundary |

### Why these differ from the "obvious" defaults

- **`crop` over `ropey`:** not "collaborative magic" (a rope is a rope; anchors are our
  layer on either). The real reason is **cheap thread-safe snapshots**, verified in the
  docs: *"Ropes use `Arc`s to share data between threads, so cloning them is extremely
  cheap."* This is load-bearing for the render model (§5) - the core clones a snapshot per
  coalesced change and hands the immutable handle to the frontend and to background
  reparse. Trade-off: `ropey` is more battle-tested (ships in Helix). `crop` is the pick
  given the snapshot architecture; `ropey` is a fine fallback.
- **`smol` over `tokio`:** an editor is I/O-bound and single-user, so tokio's
  multithreaded scheduler is not a runtime win - the smol call is about **binary size and
  build time**, both smaller. The usual objection ("smol cuts you off from the LSP
  ecosystem") **does not hold**: `async-lsp` is `tower`-based and runtime-agnostic (tokio
  is opt-in, off-by-default), and its stdio/subprocess path uses `async-process` /
  `async-io` (the smol stack). Residual risk: a future tokio-only dependency could force a
  bridge (`async-compat`). Middle path if it bites: tokio `current_thread` + trimmed
  features.
  - *Validated (M2, §14):* smol + `async-lsp` + a real `rust-analyzer` runs end-to-end
    with **no tokio anywhere in the dependency tree** (`async-lsp` with `default-features
    = false`, `client-monitor` + `omni-trait` only; process pipes via `async-process`). The
    server negotiates UTF-16 and the `lsp_rust_analyzer` test asserts the diagnostic lands
    on the right span. The residual-risk bridge (`async-compat`) was not needed.
- **`async-lsp` over `tower-lsp`:** `tower-lsp` is tokio-bound; `async-lsp` is the modern,
  runtime-agnostic choice and is what makes the smol call safe.

---

## 4. Coordinate systems (the Unicode/position contract)

An editor juggles several position spaces. Mixing them is a top source of off-by-one and
"cursor in the wrong place" bugs. **Every position-carrying type must name its space.**

| Space | Unit | Used for | Source of truth |
|---|---|---|---|
| **Byte offset** | UTF-8 byte | `crop` storage, edits, anchors | internal canonical |
| **Grapheme** | user-perceived character (`unicode-segmentation`) | cursor motion, char delete | derived |
| **Line/column** | line index + grapheme column | selections, "go to line" | derived |
| **Display column** | terminal cell (`unicode-width`, tab expansion) | rendering, mouse hit-test | frontend |
| **LSP position** | line + **UTF-16 code unit** offset | all LSP traffic | LSP spec, converted at boundary |

Rules:
- **Cursor movement is by grapheme cluster, never by byte or `char`.** Moving right over
  `👨‍👩‍👧` moves one visual step, not 7 bytes / several `char`s.
- **Display width ≠ character count.** Tabs expand to the configured `tab_width`;
  CJK/emoji occupy 2 cells. The frontend computes display columns for layout and mouse
  mapping; the core never assumes 1 char = 1 cell.
- **LSP uses UTF-16 code-unit character offsets by default** (per the LSP spec). The LSP
  layer converts between internal byte/line positions and UTF-16 positions at the boundary
  - once, in one place. (Servers may advertise UTF-8 position encoding in capabilities;
  negotiate and prefer it when available, else convert.) This is called out because it is a
  notorious "diagnostic underline is one column off" bug.
- Conversions are centralized in the buffer module and **round-trip tested** (§13).

---

## 5. Render data-flow model (how the frontend paints - the anti-Xi decision)

This is the section Xi got wrong. The rule: **the frontend must be able to scroll and
re-render any visible region with zero core round-trips.**

### Deltas are the primary output; the snapshot is a derived local convenience

The core's authoritative "what changed" output is a **delta stream**, not the snapshot. An
edit *is* a delta before it touches the rope - `Edit { buffer, range, new_text }` - and the
core is already committed to producing that exact value for three other consumers:
- the **undo tree** (§2.4) stores inverse deltas;
- the **LSP client** (§4, M2) must send `textDocument/didChange` incremental changes, which
  are deltas in UTF-16 coords;
- **partial repaint** needs the changed line range.

So deltas are first-class internally regardless. Making them the frontend seam's primary
message means **one representation of change unifies undo, LSP sync, remote sync, partial
repaint, and the journal (§8.1)** - instead of four ad-hoc mechanisms plus a snapshot
differ. This is a deliberate improvement over the earlier "snapshot is the only output"
draft, which forced `proto/` to reverse-engineer deltas back out of two snapshots -
reconstructing information the core had already computed and thrown away.

The **snapshot is derived**: it is the cheap `Arc` bundle a *local, in-process* frontend
holds so it can read any visible region synchronously (the anti-Xi mechanism). A local
terminal frontend can ignore deltas entirely and just swap to the newest snapshot; a remote
frontend consumes the delta stream directly as its wire protocol and never receives a
whole-buffer snapshot. Both are served by the same core with no reconstruction layer.

**Invariant (property-tested, §13):** applying the delta stream from version N to a
version-N buffer yields exactly the version-(N+1) buffer. Snapshot and delta stream can
never disagree.

On each *coalesced* change (not every keystroke - see coalescing below), the core emits the
delta(s) and, for local frontends, produces:

```
struct ViewSnapshot {
    buffer_id: BufferId,
    version: u64,                 // PER-BUFFER monotonic counter; frontend ignores older
    text: crop::Rope,             // Arc-shared - cheap clone (verified §3)
    selections: Arc<[Selection]>, // Arc-shared - resolved to concrete positions at `version`
    decorations: Arc<DecorationSet>, // Arc-shared - syntax/diagnostics/git/inlays (below)
    // ... line-count, dirty hint (changed line range) for partial repaint
}
```

- **Every field is cheaply shared, not just `text`.** `selections` and `decorations` are behind
  `Arc` too, so building a snapshot is a handful of atomic ref-count bumps regardless of
  file size or match count - *not* an O(spans) or O(selections) deep clone per frame. (The
  earlier draft only shared `text`; sharing just the rope while deep-cloning a
  `Vec<span>`/`Vec<selection>` would silently reintroduce per-frame cost - the exact thing
  this model exists to avoid.)
- **Decoration representation:** every overlay the core computes (syntax, diagnostics, git
  signs, inline hints) shares **one** anchor-backed channel, resolved lazily for the visible
  line range only - see *Decorations* below.
- **Frontend owns the viewport.** It holds the latest `ViewSnapshot` and, on its own render
  tick, reads exactly the visible line range from `text` + `decorations` and paints. Scrolling
  = read a different range from the *same* snapshot. **No message to the core.** This is the
  concrete mechanism that avoids Xi's round-trip-to-scroll.
- **Latest-wins:** the frontend only ever needs the newest snapshot. Intermediate ones
  during a fast paste are safely dropped (§6 channel choice makes this automatic).

**Seam-cost note (corrects the §1 "free serde derive" framing):** `Action`, `Notification`,
and the **delta stream** are all small value messages that translate to the wire
essentially for free (add `#[derive(Serialize, Deserialize)]`, channel becomes a socket).
The `ViewSnapshot` carries the whole `Rope` and does **not** serialize cheaply - but that
no longer matters, because the snapshot is a *local-only* convenience (above) and is never
sent over the wire. The remote transport ships the delta stream, which the core already
produces. This is why making deltas primary is the better design: it removes the
snapshot-diffing adapter the earlier draft needed, rather than isolating its cost in
`proto/`. Initial full-buffer sync for a newly-attached remote frontend is one `SetText`
delta variant (send the whole buffer once), after which only incremental deltas flow.

### Decorations: one anchor-backed channel, not four ad-hoc fields

Syntax highlighting is not the only thing the core computes that the frontend paints at a
position. LSP **diagnostics** (underlines + gutter severity marks), **git diff signs**
(added/changed/removed bars), and **inline hints** (LSP inlay hints, end-of-line diagnostic
messages, later AI ghost text) are the same shape: a payload attached to a buffer position
that must **survive concurrent edits** - i.e. anchor-backed (§2.1). Giving each its own
`ViewSnapshot` field would re-plumb the seam, the snapshot builder, and the render loop once
per feature. Instead there is **one** `decorations` channel carrying a typed set:

```
enum Decoration {
    Highlight   { span: (Anchor, Anchor), style: StyleId },      // syntax, semantic tokens, bracket match
    Underline   { span: (Anchor, Anchor), style: UnderlineStyle }, // error/warn undercurl, spelling
    GutterMark  { line: Anchor, kind: GutterKind },              // diagnostic severity, git add/change/del
    VirtualText { at: Anchor, placement: Placement, text: Box<str> }, // inlay hint, eol diagnostic, ghost text
}
```

- **Anchor-backed, resolved lazily for the visible range only** (the old `StyleMap` rule,
  now generalized): the `DecorationSet` stores anchors internally so decorations survive
  edits without a reparse; the frontend resolves anchors to concrete ranges **only for the
  visible line range**, never eagerly for the whole file. Snapshot construction stays
  O(1)-ish (a few `Arc` bumps); resolution cost is bounded by viewport size.
- **Producers are independent and async:** tree-sitter feeds `Highlight`s (M4), the LSP
  client feeds `Underline`/`GutterMark`/`VirtualText` diagnostics and inlay hints (M2), a
  git-diff task feeds `GutterMark`s (M8). Each lands on its own version and **may lag text by
  a frame** (pipeline below) - correct, because text is never stale and overlays only trail.
- **Styling stays frontend-owned.** A `StyleId`/`GutterKind` is a *semantic* tag (`keyword`,
  `error`, `git.added`), never an RGB color. The theme (§10.5) maps tags → concrete
  colors/attributes, so identical core output themes light/dark and truecolor/256-color
  without the core knowing terminal capabilities.
- **`Underline` is separate from `Highlight`** so one cell can carry a syntax foreground
  color *and* an independent error undercurl at once (Kitty styled underlines, §9-adjacent);
  merging them would lose that.

This subsumes the earlier `styles: Arc<StyleMap>` - which is now exactly the `Highlight`
kind - without changing its hard-won lazy-resolution and cheap-snapshot properties.

### Decoration pipeline (why decorations may lag text by a frame, and why that's fine)

Tree-sitter highlighting and LSP diagnostics are **too expensive to recompute
synchronously per keystroke**. Flow:

1. Edit applies to the buffer immediately (single owner, synchronous). Core emits a
   snapshot with **text updated now**, decorations carried forward / best-effort remapped
   through anchors.
2. A background task reparses on the cheap snapshot clone; when done, the core emits a new
   snapshot with **refreshed decorations** at a later version.
3. Result: **text is never stale** (user always sees what they typed instantly);
   highlighting may trail by a frame or two. This is the correct trade - the reverse
   (blocking on highlight before showing text) is exactly Xi's latency mistake.

### Frame budget

Frontend coalesces rapid input: it may receive many snapshots/inputs but paints at most
once per frame budget (target ~8-16ms). This is *when* it calls the loop it already owns
(§7), not a custom renderer.

*Built (M8).* A dirtied frame yields while input is **already buffered**, bounded at
16ms so a stream that never lets up still repaints rather than freezing the screen. The
loop reads one event per iteration, so without this a mouse drag - which a terminal
reports once per cell crossed - paid for a full frame rebuild per report, every one of
them showing a state the next queued event was about to replace. Measured on a 200-line
file: 300 drag reports cost 303 frames and 617KB of terminal output, against 4 frames and
7KB after. The paints were also what let the queue grow, so the gesture ran on past the
button coming up and the keystroke behind it waited for all of it.

Two things had been leaning on the old paint-per-event rhythm, and both moved to where
they belong once it went away. **The wheel's offset is clamped in the handler**, not left
to the next paint: notches that accumulate between paints run the offset past the end of
the file, and the flick back is then spent burning off an overshoot the screen never
showed. **The per-buffer viewport swap happens outside the paint**, because it is which
buffer's view state is *live* rather than which was last drawn - an event handled before
the deferred frame would otherwise read the outgoing buffer's scroll.

### Caret-follow

The viewport chases the caret when the caret has moved **or** the text changed under it,
and otherwise stays where the reader put it. Both halves matter: the caret catches
motions, and the version catches an edit that leaves the caret byte alone (deleting
forward), because typing must show you what you are typing even when the byte does not
move. Each buffer carries the version and caret byte its last frame showed, so the test
asks "did *this* buffer move while I was away" - which is what makes a switch away and
back land where you left.

*This replaced a flag that meant "this one frame".* It was set false by a wheel scroll
and reset to true after every paint, so scrolling away survived only until the next
repaint - a toast expiring, decorations landing, a buffer switched back - and then
snapped to the caret. The per-buffer scroll restore was effectively dead for the same
reason: the offset was restored correctly and then immediately overridden. A resize is
the one repaint that still pulls back without the caret moving, expressed by *voiding*
the record rather than by a second flag: a window that shrank can leave the caret below
the last row, and unlike a scroll the reader did not ask for that.

---

## 6. Channels and back-pressure

Streams and transport choices - each matched to its delivery semantics:

| Stream | Direction | Transport | Rationale |
|---|---|---|---|
| `Action` | frontend → core | **bounded** `async-channel` (small, e.g. 1024) | apply back-pressure on pathological input floods; bound memory |
| `Delta` | core → frontend | **bounded, lossless, ordered** `async-channel` | a remote frontend replays every delta in order; dropping one diverges its buffer. Local frontends may drain-and-ignore it |
| `ViewSnapshot` | core → frontend | **latest-wins single-slot** (watch-style cell) | derived convenience; frontend only wants the newest; intermediates safely dropped |
| `Notification` | core → frontend | **bounded** `async-channel` | discrete events must not be dropped, but must not grow unbounded |

The `Delta` (lossless) and `ViewSnapshot` (lossy latest-wins) streams are complementary,
not redundant: deltas are the exact ordered change log (remote wire protocol, journal,
undo source); the snapshot is the cheap "current state" a local frontend paints from
without replaying anything. A local terminal frontend typically drains `Delta` only for its
changed-line repaint hint and reads content from the snapshot.

- **Paste is one Action, not N.** A bracketed paste is delivered as a single
  `InsertText(String)` action, not a key-event per character - that is the frontend's job.
  So the real `Action`-flood source is macros/plugins/held-key-repeat, not paste. A bounded
  `Action` channel means such a producer awaits when full - natural back-pressure, no OOM.
  The core processes actions in order; the latest-wins snapshot channel means the frontend
  paints only final states, not every intermediate.
- **Frontend slower than core:** irrelevant for snapshots (it just reads the latest). For
  notifications, a full bounded channel back-pressures the core, which is acceptable
  because notifications are low-volume.
- **Cross-channel ordering is not guaranteed.** Because snapshots use a latest-wins cell
  (intermediates dropped) while notifications are an ordered queue, a `Notification` may
  arrive before/after the snapshot it relates to, or outlive a dropped snapshot.
  **Therefore notifications must be self-contained** - each carries the `buffer_id` +
  `version` it refers to and is meaningful without assuming a paired snapshot is present.
  Do not encode "this note describes the snapshot you're currently holding" semantics.
- Every channel's bound and overflow behavior is documented at its definition site.

---

## 7. Rendering: no custom render loop

Considered "keep ratatui for widgets but bypass its draw loop with a custom
frame-budgeted dirty-rect renderer to avoid tearing." **Rejected** on two verified
misconceptions, though the *goal* (no tearing) is valid.

Verified (crossterm 0.29, ratatui docs):
1. **ratatui already cell-diffs.** `Terminal::draw` keeps two `Buffer`s; "a diff is
   performed and only the changes are drawn to the terminal." A custom dirty-rect loop
   reimplements its core.
2. **No default loop to bypass.** "The onus of triggering rendering lies on the
   programmer." `loop { terminal.draw(...) }` is our code.
3. **Tearing's real cause** is the terminal painting a half-written frame, fixed by
   **synchronized output** (DEC mode `?2026`). Confirmed available as
   `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`. Terminals ignoring `?2026` silently
   no-op it, so wrapping every frame is always safe. Fallback: emit `\x1b[?2026h` /
   `\x1b[?2026l` directly.

**Approach:** own the loop; wrap each `draw` in the sync-update pair; frame-budget/coalesce
input into one `draw`. Ceiling to know: Helix runs its own compositor because
immediate-mode eventually is not enough - keep the frontend thin so replacing the renderer
stays local. Earn the compositor by outgrowing ratatui. **"Compositor" is overloaded** here:
this section defers the custom *cell renderer*; the *overlay/layer* compositor is a separate,
thinner thing that §7.5 **does** build on top of ratatui - see there.

---

## 7.5 Frontend UI architecture (compositor, surfaces, chrome)

§7 fixes *how a frame reaches the terminal* (own the loop, wrap each `draw` in sync-output,
let ratatui cell-diff). This section fixes *what the frontend draws and how those pieces are
organized* - the layer above the renderer. It is entirely frontend-owned (`vortex-tui`) and
crosses the seam only where the tables below say so.

### Two things called "compositor" - keep them separate

Helix's compositor does two jobs this spec deliberately splits:

1. **A cell renderer** (its own double-buffer + diff). We do **not** build this - ratatui
   already cell-diffs (§7), and replacing it stays deferred (§11).
2. **A layer/overlay stack** with event routing - base editor view at the bottom, floating
   surfaces (palette, picker, completion, prompt) stacked above, events delivered
   front-to-back until one consumes them. Ratatui has **no** such thing. We build this, thin.

So "compositor" in Vortex means **job 2 only**: an overlay layer manager that paints onto
ratatui `Buffer`s and routes input. This matters because §7/§11 defer "the compositor"
meaning job 1; §7.5 adds job 2, which is not that.

### ratatui widgets as primitives, our own layer stack

**Decision: paint with ratatui's widgets; build our own layer stack + event routing; do not
adopt a component framework.**

- ratatui gives paint primitives we will not rebuild: `Paragraph`, `List`, `Table`,
  `Block`/borders, `Scrollbar`, and `Clear` (the documented way to punch a hole for a popup -
  render `Clear`, then the overlay). Verified: ratatui is a widget library, not a
  component/event framework.
- ratatui does **not** give a layer stack, focus model, or event propagation. Frameworks that
  do - `tui-realm` (Elm), `ratatui-kit` (React) - are **rejected**: each is a heavy,
  opinionated runtime that owns the loop and state model, the opposite of §7's "own the thin
  loop" and §1's thin frontend. Adopting one is a dependency *and* an architecture we would
  fight.
- What we build is small: a `Layer` trait (`render(area, buf)`,
  `handle_event(ev) -> Handled | Ignored`, `cursor()`, `desired_size()`) and a `Compositor`
  holding a `Vec<Box<dyn Layer>>`. ~One file, mirrors Helix's `Component`/`Compositor` shape,
  and keeps the renderer swap (§11) local.
- Single-purpose widget crates (`tui-input`, `tui-popup`) may be pulled *if* a surface needs
  more than trivial code, but default to hand-rolling: our input surfaces are single-line and
  the real editing engine already lives in the core. **Any such crate is a §3 stack addition,
  asked-for first (CLAUDE.md).**

### UI surfaces (layers), bottom to top

| Surface | Kind | Crosses seam? | Notes |
|---|---|---|---|
| **Buffer view** | base | reads snapshot | text + decorations (§5), selections, viewport (frontend-owned) |
| **Gutter** | base | reads decorations | line numbers (absolute/relative), diagnostic severity, git signs, fold marks |
| **Status line** | base | reads snapshot | mode, position, selection count, version, diagnostic counts, LSP status |
| **Head / tab bar** | base | reads snapshot | **built (M7).** bufferline: one tab per open buffer with its modified marker, windowed around the active tab when they overflow, clickable to switch; line count keeps the right end |
| **Message / toast area** | transient layer | consumes `Notification` | **built (M6).** errors, save/LSP status, external-change notices - a real surface, not a status-bar hijack |
| **Prompt line** | overlay | emits `Action` on submit | single-line input: save-as path, search query, `:command`. Submit/cancel are the only seam traffic |
| **Command palette** | overlay | emits `Action` on pick | **built (M7).** fuzzy list of commands; nav/filter pure frontend, only the chosen intent hits the core |
| **Pickers** (file / theme / buffer / encoding / line-ending / global-search / symbol) | overlay | emits `Action` on pick | **file + theme + buffer + global-search + the two format pickers built (M7).** fuzzy list + optional preview pane (**built (M7)**, opt-in per picker, filled when the highlight *moves*; the file picker uses it, and it is dropped below 80 columns rather than halving a narrow screen into two unreadable ones); large lists stream in without blocking. Mouse-driven: a click on a row picks it, the wheel moves the highlight, a click outside dismisses |
| **Theme picker** | overlay | **none** | **built (M7).** the one surface whose commit never crosses the seam at all - chrome is frontend-owned, so it also *previews* as the highlight moves (§10.5) |
| **Global-search picker** | overlay | emits `Action` on pick | **built (M7).** the one picker whose rows are not a list it was handed: the query *is* the search, a worker thread walks the project, and results stream in through `Layer::tick`. A pick is two actions - `Open` then `PlaceCursorAt` - so it lands on the match, not the top of the file |
| **Message log** | overlay | none | **specified (M10).** the retained ring the toast is a 4-second preview of - the same picker surface, so it costs a command rather than a widget. What makes an LSP spawn failure reportable at last (§14 M2) |
| **Diagnostics picker** | overlay | emits `Action` on pick | **specified (M10).** the head bar's `✗3` is the control that opens it, and it jumps like the global-search picker does. A count that names a problem is also the way to reach it |
| **Empty-state hints** | base | none | **specified (M10).** three chords, rendered from the keymap (§10.5), on an empty pathless buffer only, gone on the first keystroke |
| **Which-key popup** | overlay | none | **deferred out of M7 (§11).** It is the one surface with a prerequisite it cannot supply itself: "the available continuations from the keymap" presupposes a keymap that *has* continuations, and this one is flat by design (§10.5). Revisit with chord sequences, not before |
| **Completion popup** | overlay | emits `Action`, reads decorations | LSP completion menu; ghost-preview of the selected item as a `VirtualText` decoration |
| **Hover / diagnostic popup** | overlay | reads decorations | LSP hover + full diagnostic text on demand |

**Seam rule for surfaces:** navigation *inside* a surface (moving the palette selection,
typing in a picker filter) is **pure frontend** and never round-trips to the core - the same
anti-Xi rule as scrolling (§5). Only the *committed intent* (the picked command, the
submitted path, the accepted completion) becomes an `Action`. Fuzzy matching runs
frontend-side, on `nucleo-matcher` (Helix's matcher; §3), which landed with the pickers.

### What the chrome says (the information architecture, M10 - specified)

The table above says *which surfaces exist*. This says **what each one is allowed to put on
screen**, which is a different question and had never been answered: the head bar's right
segment holds a line count, the status bar carries an internal document version, and three
things the editor knows - the diagnostic count, the language server's health, and which
grammar is attached - are on screen nowhere at all.

**The rule, and it settles most of the argument:** *a chrome cell is spent on what the user
cannot otherwise learn - and a readout that is always present stops being read, so presence
itself is the signal.* The line count fails it, because the gutter already counts lines and
the scrollbar already shows position. The document version fails it. The byte size fails
it, because the filesystem answers it. Encoding passes, because the buffer holds UTF-8 with
LF whatever the file holds (§10.1), so nothing else on screen can tell you the file is
CP-1252. Diagnostic counts pass. Server health passes. Indentation passes, because it
silently decides what the Tab key inserts.

The second half of the rule is what keeps the bars quiet. A segment reading `0 errors`
forever teaches the eye to skip the place errors will appear, so a segment is **absent at
nominal and present when it has something to say**, in a slot it never leaves.

**Three zones, one question each.** The split is what stops the bars and the toasts
duplicating each other:

| zone | answers | carries |
|---|---|---|
| head bar | *what am I in?* | identity and its condition - buffer, branch, language, health |
| status bar | *where am I, and how is this written?* | caret position, and the format a save will reproduce |
| toast + log | *what just happened?* | events, never standing conditions |

From which: **an event that leaves a standing condition toasts once, then lives in the bar
and in the log.** The bar never re-announces; the toast never reports something that is
still true and already shown.

#### The head bar

Tabs keep the left, as M7 built them, plus one fix: **colliding names take the shortest
parent that separates them** (`tui/layout.rs` beside `core/layout.rs`), never two tabs both
reading `layout.rs`. The right becomes a state cluster, and the *presence* rule is what
decides each member - this is where the design first went wrong and was corrected, so the
reasoning is recorded rather than the outcome alone:

- **`⎇ main*`** - always, inside a repository. The one identity fact a full-screen editor
  hides from you, and a wrong-branch edit is expensive. First to drop under narrowing.
- **`◐ indexing` / `✗ no server`** - only while the server is **not** ready. A permanent
  `● rust-analyzer` would spend fourteen columns reporting that nothing is wrong, which is
  the rule's own violation.
- **language** - only when the grammar **disagrees with the file name**: a failed `dlopen`
  (§14 M4) reads `plain`, an override reads what it overrode to. `sample.rs` opened by the
  Rust grammar needs no label; the name already said it.
- **`✗3 ⚠1`** - flush right, absent at zero, and **never dropped**. Of everything on this
  bar, a count of errors is what a user must not have to widen a window to discover. It is
  the head bar's twin of `[read-only]` leading the status bar's left segment.

**The width budget is 80 columns** - already the width at which the file picker drops its
preview pane, so the editor has one narrow-terminal number rather than two. Drop order,
left to right: branch, then server (which keeps its glyph and loses its word below ~90),
then indentation, then encoding. Problems and caret position survive every width. The first
draft of this design was laid out against a 100-column capture and **did not fit in 80**,
which is why the budget is written down here instead of being rediscovered per feature.

#### The status bar

| segment | now | then | why |
|---|---|---|---|
| `[read-only]` | leads the left | keep | Survives every truncation, which is the point of its placement |
| position | `Ln 22, Col 1` | keep | The reason the bar exists |
| selection size | `(14 selected)` | keep | Already obeys the presence rule |
| **cursor count** | - | **add** | Cursor state is a `SelectionSet` (§2.2); the bar reports the primary as though cursors were singular. Above one it says `3 cursors` |
| **indentation** | - | **add** | Invisible otherwise, and it decides what Tab inserts |
| encoding · EOL | `UTF-8 · LF` | keep | Unlearnable from the buffer, and already clickable (§10.5) |
| byte size | `749B` | **drop** | The filesystem answers it; never the question mid-edit |
| version | `v1` | **drop** | Instrumentation. §5 scoped it to "while the delta/version model is young"; it is no longer young |
| **caret diagnostic** | - | **add** | Takes the right segment once the caret has *rested* on a flagged line |

**The rest rule is not a refinement.** Without it the diagnostic replaces the encoding
readout on every keystroke that crosses a bad line, and a strobing segment is worse than an
absent one. It reuses the 150ms `HIGHLIGHT_WAIT` the global-search debounce already
established (§11), so the editor has one "the user has stopped" constant, not two.

**Indentation carries new scope, and it is the only new scope here.** `spaces:4` is a
readout of a setting that does not exist - `insert_tab` always inserts a tab. The
*frontend* already decides what that command inserts (§10.5: a tab is one byte whatever it
is painted as), so an `indent_style` is a frontend change, not a core one.

#### A count that names a problem is also the control that reaches it

`✗3` is clickable and opens a **diagnostics picker** over the shared picker surface, and
`next_diagnostic` / `previous_diagnostic` join the command vocabulary. This generalizes a
decision the status bar already made (§10.5): clicking the encoding readout opens the
encoding picker, *because the place that shows the answer is the place to change it*.
Reporting three errors and offering no way to reach them is half a feature.

#### One dialog anatomy

Every overlay is the same shape, so the surface is learned once: **title and result count
in the border** (free real estate - it costs no row), a query row, rows that show *why*
they ranked, an optional preview pane, and a hint footer. Three of the four are new:

- **The count** (`9 of 240`) is what tells you whether to keep typing or start arrowing.
- **Matched characters are marked.** `Pattern::indices` returns the positions beside the
  score, so this is a call, not a second pass. Unmarked, a ranked list looks arbitrary.
  Two details it costs, both found by building it: the indices are **char** positions in
  a `Utf32Str`, so the paint walks chars while accumulating *display* width - a row with
  a wide glyph would otherwise drift a cell per character past it. And `palette_match`
  sets a foreground and no ground, so a mark keeps whichever row style is underneath and
  the highlighted row stays one unbroken band - the rule the indent guide already follows
  over a selection wash.
- **Path rows read as paths** - directory dimmed, file name in full ink.
- **The hint footer is generated from the keymap**, never written as a literal. That is the
  §10.5 rule M9 establishes, and it is why M10 follows M9 rather than preceding it.

*Rejected:* a **frameless panel on a dimmed backdrop** - handsomer, but it needs a dimming
slot in all four themes and collapses to nothing on a 16-colour terminal, and Phosphor has
exactly one hue to spend (§10.5 theme files). A **full-width query bar with the list
beneath** - more room per row, less obviously modal, and an overlay that is modal over the
keyboard should look it.

**Confirmations stay a single bottom line.** A modal box for one keypress weighs more than
the question. Two changes only: the answers render from the `confirm` context's bindings
(M9), and the destructive answer is *marked* rather than merely capitalized.

#### Messages

The toast stays; what it loses - everything, after 4 seconds - gets a **message log**: the
existing picker over a retained ring, timestamped and severity-marked. One command, no new
widget, and it changes what the editor is permitted to tell you. M2 deferred surfacing an
LSP spawn failure precisely because reporting it meant shouting once and vanishing; with a
log it can toast once, sit in the head bar as `✗ no server`, and stay in the log.

One placement rule the toast lacks today: **it never covers the caret's own row.** The
stack sits top-right and drops to bottom-right when the caret is on a row it would cover.
The frontend knows the caret's screen row, so this is a comparison, not a mechanism.

*Rejected:* moving messages into a middle zone of the status bar. That re-hijacks the bar
the toast surface was built to stop hijacking (§7.5 surfaces table, M6).

#### The empty state

`vortex` with no file opens an unnamed buffer and says nothing else - the first screen a
new user sees, undesigned. It gets three centred, dim lines: open a file, the command
palette, quit, **with their chords rendered from the keymap** like every other chord (M9).
Shown only for an empty buffer with no path, and gone on the first keystroke: a splash
screen you have to dismiss is a splash screen that was not worth showing.

#### Glyphs, and the terminals that lack them

**Shape carries the meaning; colour reinforces it.** The counts read `✗3 ⚠1` and never a
red 3 beside an amber 1. Instrument is the proof this is a constraint and not a courtesy -
it is achromatic and spends its single hue on errors alone (§10.5) - and the same rule
serves a colourblind reader and a 16-colour terminal, which is why it is a design rule
rather than an accessibility footnote.

**A font profile, not a hope.** `⎇` and `◐` are absent from many terminal fonts, and a
missing glyph renders as a box that misaligns every cell after it. A `glyphs =
"unicode" | "ascii"` config selects a full replacement set (`●`→`*`, `✗`→`E`, `⚠`→`W`,
`◐`→`~`, `⎇`→`git:`), every substitute **one cell wide**, since a two-cell fallback would
move the text under it - the same width discipline §4 imposes on the buffer.

#### The body

Almost nothing: it is the part that already works. **Severity glyphs in the gutter**, which
the decoration channel has produced since M2 (`GutterMark`) and the painter has never
drawn. Inline end-of-line diagnostic text stays deferred - it wants the `VirtualText`
decoration §11 already names.

**Deliberately not designed here:** the completion popup and the hover panel. Those are LSP
work, not chrome work - the client sends `didOpen`/`didChange` and consumes
`publishDiagnostics`, and nothing else (§14 M2). Drawing their panels before the requests
exist would be framing an empty room.

### Chrome and polish (frontend-owned, incremental)

Each reads data the snapshot/decorations already carry; none needs a seam change beyond the
decoration channel. All are theme-driven (§10.5) - theme *files* exist now, so a new piece
of chrome adds a slot to that format rather than a constant - and default-off where they
add noise, each on/off switch a key in the config file the M5 loader now reads:

- **Indent guides** - *built (M8).* `indent_guides = true` in the config file, plus
  `toggle_indent_guides` for mid-session. A rule at every tab stop strictly inside a
  line's indentation, so a line indented 8 with 4-wide tabs carries guides at columns 0
  and 4. **Column 0 is included**: it is the left edge of the block the indented text
  sits in, which is the one level nothing else on screen shows, and dropping it would
  leave a singly-indented line with no guide at all.
  Painted as a **glyph, not a ground tint** - the opposite call from the ruler above,
  and for the reason that made the ruler a tint: the cell a marker wants decides what it
  can be. A ruler's cell is the one a long line is already using, so a glyph there would
  displace text; a guide's cell is by construction *indentation*, so its glyph stands in
  for a space, takes the same one cell, and nothing after it moves. The substitution is
  into the row's painted text, never into the line the core holds, so every byte↔column
  mapping still measures the buffer's own bytes rather than what the row shows. Its theme
  slot (`indent_guide`) sets a foreground only and no ground, so a selection or a
  current-line tint flows over a guide instead of being broken by it - the glyph survives
  the wash, which is what a tint could not have done.
  The overlay is pushed **with the syntax highlights, not with the rulers**, and that is
  the correction worth recording: a guide is the same *kind* of thing a highlight is, a
  foreground over whatever ground the washes have laid down. Ordered under the rulers'
  rule it lost its color to any selection crossing it, since the theme's `selection` sets
  a foreground too - and a guide at full selection brightness stops reading as a margin
  and starts reading as a `│` the user typed. Its ground is untouched either way, so the
  selection still owns the cell; only the glyph keeps its dimness.
  **Indentation here is spaces and tabs, not every character Unicode calls whitespace**,
  and the narrowness is what makes the two halves agree. `expand_tabs` turns a tab into
  spaces, so a prefix of spaces and tabs is a prefix of *spaces* by the time it is
  painted, and every column the guide computation hands to the substitution is therefore
  a cell holding `' '`. Counting a NO-BREAK SPACE would break that: the column would land
  on a character the glyph cannot replace, leaving the cell recolored but unmarked - a
  dimmed character where a rule should be. A line indented with those is simply not
  indented as far as the guides are concerned.
  **A blank line inherits the shallower of its nearest non-blank neighbours.** This is
  not a refinement: a run of blank lines is what punches a hole through every guide
  crossing it, and blank lines between statements are the common case, so without
  inheritance the feature looks broken exactly where it is most used. Taking the
  *shallower* side is what stops a guide running past a closing brace - the blank line
  trailing a block belongs to the block outside it, not the one that just ended. A
  missing neighbour counts as column 0 rather than "use the other side": nothing encloses
  the top or the bottom of a file. The search answers from the visible window where it
  can and crosses into the rope only at the window's edges, resolved once per frame - a
  blank row at the top of the screen must still inherit, or guides would flicker as you
  scrolled - and the part that leaves the window is **bounded** (§10.4): a blank row
  whose nearest non-blank neighbour is further off-screen than that gets no guides,
  which is the honest answer, since "what block is this inside" has stopped being a
  question the eye is asking. The bound is on the outside search *only*; the search
  within the window is already bounded by the viewport's own height, which is what
  §10.4 is actually about.
  **The guides a row is offered are clipped to the horizontal window before anything
  paints them** (`guides_in_window`), and this is §10.4 biting on a surface that looked
  exempt from it. A guide is one per tab stop of a line's *indentation*, and indentation
  is a length the **file** chooses - so a line carrying thousands of columns of leading
  whitespace hands the frame thousands of guides, and pays for them twice: once walking
  cells in the substitution, and again in `render_line`, whose `style_at` scans every
  overlay for every painted cell. To draw none of them. Clipping is a subslice rather
  than a filter, since the columns are ascending, so it costs two binary searches and no
  allocation. Measured on the bench that ships with it (`indent_guides/raw` against
  `/clipped`): at a 16 384-column indent the unclipped substitution is 186 µs per row
  per frame and the clipped one 1.1 µs, and the clipped series stays flat from 16
  columns to 16 384 while the raw one grows linearly. The clipping leaves **no trace in
  the painted output** - it removes work, not marks - which is exactly why it is a named
  function with its own test rather than three lines inside the paint loop: nothing
  downstream can observe whether it happened.
  The substitution itself walks `columns` with **one cursor** rather than searching it
  per cell (the trick `ColumnWalker` plays for syntax spans, and for the same reason:
  the query sequence is monotonic). Without that it was quadratic in the indent - 4.4 ms
  per row at 16 384 columns - which is the shape a security review flagged as the one
  paint input a file can size.
- **Relative line numbers** - *built (M8).* `line_numbers = "absolute" | "relative"` in
  the config file, plus `toggle_line_numbers` for mid-session (named like any other
  command, palette-listed, and left unbound - chrome switches earn a chord only if they
  are used enough to deserve one). There is deliberately **no third "pure relative" mode**
  printing `0` on the caret's row: the number a relative gutter exists to give you is the
  count you type before a motion, and the one row that never needs it is the row you are
  on - so that slot carries the absolute number instead, which is the thing relative
  numbering otherwise costs you. The field is sized from the buffer in **both** modes,
  never from the numbers relative mode happens to print: a gutter that narrowed to fit
  them would resize every time the caret crossed a power of ten, sliding the whole text
  body sideways under the reader.
  **The origin is the caret, including when the caret is off screen** - after a wheel
  scroll or a scrollbar drag, which the view now *stays* at (see "Caret-follow" below),
  or while a search preview holds the viewport on a match the caret has not moved to
  (§11). The gutter then reads as large distances with no absolute number anywhere on
  screen, which is worse than absolute mode would be, and it is still right: the caret is
  where an edit will land, and a count typed before a motion is counted from *there*, not
  from whatever the viewport happens to be showing. Numbering from the top visible row
  instead would make the gutter agree with the screen and disagree with the keyboard. The
  current-line tint keys off the caret for the same reason and is likewise invisible while
  a preview is scrolled away, so the two stay consistent.
- **Cursor shape per mode** - bar/block/underline via the terminal cursor (the frontend
  already drives the real cursor).
- **Rulers / colorcolumn** - *built (M8).* `rulers = [80, 100]` in the config file, a
  list because the limits a file is held to come in pairs as often as not, and drawing
  several costs what drawing one does. Columns are **0-based**, so `80` marks the first
  column *past* an 80-column limit - where the limit is actually crossed, not the last
  cell inside it. Painted as a **ground tint, not a glyph**: a character would have to
  displace the text it sits under, and the cell it wants is exactly the one a long line
  is using. The tint is a theme slot (`ruler`) held distinct from `current_line`, since
  the two cross on the caret's row and a ruler that matched would disappear precisely
  there. Rulers seed each row's overlay list, so **everything paints over them** - a
  selection or a search match crossing a ruler must not be the thing that gives way -
  and they extend past a line's end onto the padded cells, because a ruler marks a limit
  a short line has *not* reached rather than one it has. They stop at the end of the
  buffer: a ruler marks a column of a line, and past the last line there is no line to
  hold one.
- **Scrollbar** - *built (M8).* `scrollbar = false` in the config file to decline it,
  plus `toggle_scrollbar`. ratatui's `Scrollbar` on the body's right edge, as this
  section said - the one place a shipped widget was the whole answer, since the layer
  stack we build is job 2 and a bar in a column is job 1's business.
  **It is the one piece of chrome here that defaults to on**, having shipped off. The
  rest of this section's chrome marks cells the text was already using, so it stays off
  until asked; this marks none, and it answers a question nothing else on screen does -
  where the *window* sits in the file, as against the status bar's line number, which
  says where the caret sits. Those are different questions, and only one of them was
  being answered. A column is the price, and the setting still buys it back.
  **The column is reserved whenever the setting is on, painted or not.** This is the
  decision the feature turns on: the tempting version claims the column only once a file
  outgrows the viewport, and that version slides every line one cell sideways at exactly
  the moment the file crosses that boundary - a reflow triggered by typing. So the text
  width is `body - gutter - 1` for the whole session, and what comes and goes is only
  what is drawn *in* the reserved column. Nothing is drawn there when the buffer fits: a
  bar answers "where am I in something bigger than the screen", and with nothing bigger
  there is nothing to answer - a full-height thumb would be a loud way of saying so.
  Its `content_length` is the number of scroll **positions** (`max_scroll + 1`), not of
  lines, with `viewport_content_length` set to the visible rows. That is what makes
  ratatui's own geometry produce a thumb covering exactly the fraction of the track that
  the screen covers of the buffer, at exactly the fraction the offset has travelled;
  passing the line count instead yields a half-height thumb for a file that just fits.
  **It takes the pointer** (§2.2's gesture rules): a press on the track throws the view
  there, a drag follows it. The row→offset map is *linear over the whole track*, so the
  last row reaches the last line - the obvious alternative, putting the thumb's *top*
  under the pointer, can never reach the bottom, since the thumb's own height is always
  left over below it. That is not at odds with the feel of grabbing the thumb: under this
  map the pointer sits inside the thumb at every offset, up to the point where the buffer
  passes `track²` lines and the thumb collapses to its one-cell floor, past which no
  single cell can stand for a viewport and the two roundings have nothing left to agree
  on. Reaching both ends is what is worth keeping there.
  A press decides whether the gesture belongs to the bar and a **drag inherits that
  answer** rather than re-asking, so pulling a cell sideways off the column keeps
  scrolling instead of becoming a text selection halfway through. That is the second
  piece of pointer state in the frontend (after `click::Clicks`), and it exists for the
  same reason: terminals report positions, not gestures. A track too short to tell its
  offsets apart - a single row, on a three-line terminal - answers *nothing* rather than
  `0`: throwing a reader at line 900 back to line 1 for touching the only cell the bar
  has is worse than the press doing nothing at all.
  The reserved column also carries the **row's own ground**. `render_line` pads only to
  the text width, which the column is outside of, so without that the current-line wash
  would stop one cell short of the body's edge and the caret's row would show a notch -
  and the gutter already takes the tint, so the row would be washed at both ends and
  broken at one. The bar paints over it afterwards and its styles set a foreground only,
  so the track and thumb land on the row's ground rather than punching the hole back in.
  **The picker overlay carries the same bar**, on the same terms: a column reserved at
  the right of its list whether or not a bar is drawn in it (a list overflows and stops
  overflowing *as you type*, so a column that came and went would re-clip every label on
  the keystroke that changed the match count), painted over the list rows only and not
  the query line, and taking the pointer the same way. The widget and its state are built
  in one place (`layout::scrollbar`) for both, because the `max_scroll + 1` /
  `viewport_content_length` pairing above is the exact geometry `scroll_at_track_row`
  inverts on the way back - split across two files, the map and its inverse could drift
  with no compile error and leave the thumb somewhere other than under the hand pulling
  it. Two things differ. The overlay's bar is **not** gated on `scrollbar = true`: that
  key buys a permanent column off the text, which is the thing worth declining, while a
  box that is on screen for a keystroke or two costs nothing to leave; a second key for
  it would be a setting nobody wants to hold an opinion about. And its drag moves the
  **highlight**, because a picker's offset is derived from the selection (one source of
  truth, so a click cannot land on a row other than what is drawn) - which puts the
  highlight on the window's last row, exactly where the wheel already leaves it. Preview
  work is held until the button comes up: a drag reports an event per cell crossed, and
  a preview reloads a theme while a pane reads and decodes a file (a megabyte of one for
  the project-search picker), so the gesture buys one read rather than one per report.
- **Sticky context header** - *built (M8).* `sticky_context = true` in the config file,
  plus `toggle_sticky_context`. The first line of every scope enclosing the viewport's
  top row, pinned above the text, outermost first, each row carrying its own line
  number - which is what makes the header a set of jump targets rather than a caption:
  a press on one goes to the line that opened that scope.
  **It needs the parse tree, which the highlighter cannot hand over**, so the producer
  parses the source a second time (`syntax::scope`, driven by a per-language
  `context.scm`). `tree_sitter_highlight::Highlighter::highlight` takes source bytes and
  returns an event stream; there is no way to reach the tree it built or to give it one.
  The alternatives were re-implementing highlighting over a raw `Query` - re-deriving
  the locals and injection machinery M4 already has working - or a second parse. The
  second parse is off the keystroke path, coalesced by the same drain-to-newest that
  keeps fast typing from queueing parses, and **paid only by grammars that ship a
  `context.scm`**: an empty context query means the producer does no scope work at all.
  It collapses back to one parse if incremental reparse (§14, deferred in M4) lands,
  since that change means owning the tree here anyway.
  **The whole file's scope ranges cross the seam, not the answer for one row**, because
  the row in question changes on *scroll* and scrolling is frontend-owned (§5) - asking
  the core would put a round-trip on the scroll path, the same reason the search preview
  stays frontend-side. They ride the decoration channel as `Decoration::Scope`, so
  anchors carry them across edits and the header keeps naming the right function while
  you type. It is the one decoration not painted *at* its position; it is on this channel
  because it is a position that must survive concurrent edits, which is what the channel
  is for. Its own **bucket** (`DecorationSource::Scope`), separate from the highlights
  the same producer publishes, and that separation is load-bearing rather than tidy:
  scopes are *nested* by construction, so their ends are not monotonic, while
  `highlights_in` binary-searches a bucket on exactly the sorted-non-overlapping
  invariant highlights have. Mixing them would misplace the highlight search, not merely
  slow it.
  **The header's height and the scroll offset are each other's input** - the header pins
  what encloses the top line, and the top line is what the header's rows push down to -
  so a frame settles them together (`STICKY_SETTLE_PASSES`) rather than letting them
  chase each other across repaints. Two passes is the fixed point whenever the view is at
  rest; the third is a bound, so a pair that would oscillate stops a row off instead of
  spinning. The height is **dynamic**, capped at `STICKY_CONTEXT_MAX` rows and at a third
  of the body: reserving a fixed block would spend rows on blank chrome at the top of a
  file, and a file's nesting depth is a length the *file* chooses (§10.4). Over budget,
  the **outermost** rows are dropped - the row a reader needs is the function they are
  inside, not the module three levels out.
  The text window shrinks by exactly the header's height, so **no line is ever behind a
  pinned row**, and following the caret is therefore just `scroll_to_show` in the shorter
  window. The tempting extra step - shifting the followed line up by the header's height
  to "clear" it - pushes the caret that many rows off the *bottom*, since the rows it
  clears are ones the text no longer occupies. That shipped in a first draft, passed a
  test asserting the wrong invariant, and was caught by driving the editor in a pty.
  Scope ranges confined to one line never cross the seam: once such a line is above the
  viewport, so is all of it, and nothing about it encloses what is on screen.
  The scrollbar, when both are on, is painted over the **text** area rather than the whole
  body - the track stands for what scrolls, and the pinned rows do not - which is also
  what keeps a press on the bar's column above the text from throwing the view to the top
  of the file.
- **Current-line tint, selection wash, multi-cursor carets** - already built (M1-M3); listed
  so the catalog is complete.

---

## 8. Failure model

Editors that lose data or hang on a subsystem failure are unacceptable. Rules:

- **`vortex-core` is `Result`-typed throughout** with `thiserror` error enums. No `unwrap`/
  `expect`/`panic!` on buffer, file I/O, LSP, or config paths (mirrored in CLAUDE.md).
- **Save failure** (permissions, disk full, read-only): never lose the buffer. Surface a
  `Notification::Error`; keep the buffer dirty; offer save-as. Prefer atomic write
  (temp file + rename) so a failed write cannot corrupt the original.
- **LSP server crash / non-response:** the LSP task isolates the failure, emits a status
  notification, and **restarts with exponential backoff** (capped). Editing never blocks on
  the LSP being alive - LSP is strictly additive.
- **External modification** (file changed on disk, via `notify`, §10.2): if buffer is
  clean, reload and emit a status note; if dirty, emit a conflict `Notification` and let
  the user choose (keep / reload / diff). Never silently overwrite either side.
- **Panic isolation:** the core actor runs so that a panic in a non-critical task (future
  plugin, syntax) is caught (`catch_unwind` at task boundaries) and downgraded to a
  notification rather than taking down the editor. A panic in the core actor itself is a
  bug - fail loudly in debug.
- **Crash safety via the Action journal (§8.1):** rather than periodic buffer dumps, append
  the `Action`/`Delta` stream to a journal and replay it after a crash. Design room left
  now; the journal file format lands post-MVP.

### 8.1 The Action journal (one mechanism, three payoffs)

`Action`s (and the `Delta`s they produce) are serializable anyway - that is the
remote-frontend requirement (§1). Appending every *applied* action to a per-session journal
file is nearly free on top of that serialization and buys three otherwise-separate
subsystems at once:

- **Crash recovery.** Replay the journal from the last save to reconstruct unsaved work -
  the role Vim's swap files play, but as an intent log (small, append-only) rather than a
  buffer dump. Cleaner and it composes with undo.
- **Record / replay debugging.** "Reproduce the bug" becomes "replay this action log" -
  directly serving the CLAUDE.md reproduce-before-fixing rule. A user bug report can ship
  its journal.
- **Test corpus.** Real journals drop straight into the §13 interaction-test harness as
  golden cases, because that harness already replays `Action` sequences.

The journal is post-MVP, but `Action`/`Delta` are designed serializable from M1 so it (and
the remote frontend) ride along for free rather than forcing a later retrofit.

---

## 9. Input: enable the Kitty keyboard protocol up front

Classic terminal input cannot distinguish `Ctrl+I` from `Tab`, `Ctrl+M` from `Enter`, or
report key *releases* - crippling for rich keymaps. The **Kitty keyboard protocol** reports
full modifiers + key events; crossterm can negotiate it where supported. Enable at startup
(with graceful fallback where unsupported) so keybindings are not arbitrarily limited.

*Verify the exact crossterm API for the Kitty enhancement flags against current docs when
wiring input.*

---

## 10. File handling contract

### 10.1 Encoding + line endings
- **Detect encoding on load** (`encoding_rs`; BOM sniff + heuristic). Decode to UTF-8 for
  the internal rope. **Remember the original encoding** and re-encode on save (do not
  silently rewrite a Shift-JIS file as UTF-8).
- **Detect line ending** (LF / CRLF) on load; store the dominant style; **preserve it on
  save**. Internally normalize to LF for editing logic, re-apply on write.
- **Final-newline policy** configurable (default: ensure trailing newline on save,
  POSIX-style), and never reported as an unsaved change spuriously.

### 10.2 External changes
Watch open files with `notify`; behavior per §8 (clean → reload, dirty → conflict prompt).

### 10.3 Read-only + special files
Detect read-only (permissions) and surface it; refuse edits or mark buffer read-only.
Handle non-regular files (fifos, `/dev/*`) defensively.

### 10.4 Large files - tiered commitment
"Support huge files" splits into three tiers with very different costs. **We commit to
Tiers 1-2 and make Tier 3 a swap-ready seam, not a built feature** (same move as CRDT in
§2.1). Rationale: multi-GB work is overwhelmingly *viewing* (search/tail/navigate), not
free random-access editing, and Tier 3 collides with the §5 render model (see §11).

| Tier | Size | Use case | Our stance |
|---|---|---|---|
| 1 - large source | ≤ ~few hundred MB | generated code, vendored bundles, big JSON | full support; in-RAM `crop` |
| 2 - big data/logs | ~100 MB - few GB | logs, CSV, DB exports | in-RAM, **degraded features** |
| 3 - bigger than RAM | 10 GB+ | rare; almost always viewing | **deferred seam** (§11), not built |

- **Tiers 1-2 are just "never do anything O(n) on the hot path."** `crop` keeps edits
  O(log n) at any in-RAM size; the failure modes are all full-file scans. Invariants,
  enforced from day one (cheap now, painful to retrofit):
  - **Lazy/background line indexing** - never eagerly scan the whole file for newlines on
    load.
  - **Sampled encoding + line-ending detection** - BOM + a bounded prefix sample, not a
    whole-file pass (§10.1).
  - **Viewport-bounded syntax** - tree-sitter parses/highlights around the visible region
    incrementally, never the whole buffer up front.
- **Degradation threshold** (Tier 2, configurable, default ~256 MB): warn, disable
  tree-sitter + LSP for that buffer, open read-only or degraded. The cost at this size is
  syntax/LSP, not the rope - so degrade *those*, not the buffer.
- **Tier 3 is a different buffer architecture** (piece table over mmap / paged virtual
  buffer), deliberately not built - see §11 for why it conflicts with §5.

### 10.5 Configuration (styles + keymap)
User configuration is **frontend-owned and file-loaded** (`toml` + `serde`, Helix-style;
§3). The core stays config-free: chrome styling and key bindings are pure frontend
concerns (§2.2, §5) and never cross the seam - with the one exception the loader below
names. Two surfaces were configurable from the start of the design, before the file
existed to configure them from:

- **Styles (theme).** Two frontend-owned tables. (a) Colors/attributes for the non-text
  chrome - editor ground, head bar, status bar, line-number gutter (active vs inactive),
  selection, carets, toasts, overlay panels. **Built and shipping** - see "Theme files"
  below. (b) The **semantic tag → style** map the decoration channel needs (§5):
  `StyleId`/`GutterKind`/severity tags (`keyword`, `error`, `git.added`) resolve to
  concrete colors/attributes *here*, on the frontend, so identical core output themes
  light/dark and truecolor/256-color. Not built - it arrives with the decoration channel
  (M2/M4) and extends the same file format with a second table. Terminal-capability
  fallback (truecolor → 256 → 16, styled-underline support) lives in this resolution
  step, never in the core.
- **Keymap.** The key→intent table (§2.2, §12.2) is **data, not code**: a `Keymap` is a
  set of `(chord → command)` bindings, and key translation is a pure lookup over it. Both
  sides parse from strings, so the built-in defaults are expressed in the same form a
  config file uses (`Keymap::from_pairs`), guaranteeing the format round-trips:
  - **Chord grammar:** `mod+mod+key`, modifiers `ctrl`/`shift`/`alt` in any order,
    case-insensitive (e.g. `ctrl+s`, `shift+right`, `pageup`). A single character is a
    `Char` key; named keys cover the non-text keys.
  - **Command names:** stable identifiers (`quit`, `save`, `delete_backward`,
    `insert_newline`, …). Motions use a `move_<kind>` / `select_<kind>` scheme where
    `select_` is the selection-extending variant (`move_line_start`, `select_page_down`),
    so **`extend` is part of the binding, not a runtime modifier** - `right` and
    `shift+right` are distinct entries.
  - **One namespace, one table - including overlay triggers.** Commands that open a
    frontend surface (`open_palette`, `open_file_picker`, `open_theme_picker`, §7.5) are named and bound
    exactly like editing commands, in the same table, even though they resolve to a
    frontend-local effect rather than a core `Action`. A separate map for them would be
    a second thing `from_pairs` has to populate, and since the config path *is*
    `from_pairs`, anything it missed would silently vanish the first time a user wrote
    a config file. The same identity is what the palette lists and looks shortcuts up
    by, so a command's name, its binding, and its palette row cannot drift apart.
  - **Command names are a public contract** once a config file exists: renaming one
    breaks user configs. They are cheap to settle now and expensive later, so new
    commands should be named deliberately rather than after the fact.
  - **Text entry is a fallback, not a binding:** an unbound printable char with no Ctrl
    inserts itself, so the map never enumerates every letter.
  - **Open:** modal-vs-modeless is the remaining design question, drafted alongside the
    §12.2 `Action` vocabulary. Chord *sequences* and per-mode maps stay deferred with
    their triggers (§11); everything else the table owes is settled in "Every binding is
    data" below.

#### Every binding is data (M9 - built)

M0-M8 made the *editor's* bindings data and left everything else in code. Five surfaces
match key codes directly in a `match key.code` of their own - the pickers, the prompt, the
find prompt, the confirmation and the query-replace walk - so not one of their keys can be
rebound. A default binding can be replaced but never removed, so `ctrl+f` cannot be freed.
The platform split lives in `cfg!`. And the defaults themselves are a Rust table no user
can read. M9 closes all four, under one rule: **a key that does something has a name and a
table row**, and what stays in code is only what a table cannot say (listed at the end).

Two stages, in order. Stage 1 needs no new machinery and is worth shipping alone.
Both are built; what building them settled is recorded with M9 in §14.

**Stage 1 - the table gets what every editor's table already has. Built** - the two
details this design left open (the one macOS-only binding, and which keymap `--help`
renders against) are recorded with M9 in §14.

- **`mod` is the platform command modifier**: Cmd on macOS (crossterm `SUPER`), Ctrl
  elsewhere, resolved by `Chord::parse` at parse time. `"mod+c" = "copy"` is then one row
  that means the right thing on both, and `COMMAND_MOD_BINDINGS`' `cfg!` split disappears
  into it. `Chord::display` renders the *resolved* modifier (`Cmd+C` on a Mac), because
  the palette must show the key the user actually presses. Zed's `secondary-` is the same
  device.
- **`nop` unbinds.** `"ctrl+f" = "nop"` binds the chord to nothing and - the part that
  matters - **consumes it**: the lookup returns before the text-entry fallback, which
  would otherwise turn an unbound printable chord back into typing. Helix's `no_op` and
  Zed's `null` are the same answer; TOML has no null, so it is a command name. Under
  contexts (Stage 2) `nop` is also how a config says "swallow this here", as distinct from
  "let it fall through".
- **The defaults are a file.** `crates/tui/keys.toml`, compiled in with `include_str!`
  beside `themes/*.toml`, parsed by the same loader the user's file goes through. The Rust
  tables are *deleted* rather than mirrored, so the two cannot drift and no equality test
  is needed - unlike `Theme::default()`, which keeps its hand-written twin because a theme
  must exist before any parsing can fail, a bootstrap need a keymap does not have (a
  `keys.toml` that will not parse is a build-time bug the `Keymap::default` test catches).
  It also answers a question the editor cannot answer today - *what are the defaults?* -
  since the file is the reference a user copies a row out of.

**Stage 2 - contexts, so the surfaces are bindable too. Built.**

A **context** is a name. At any instant an ordered list of them is active, lowest first:

1. `editor`, always - the `[keys]` table itself.
2. the platform - `macos`, `linux` or `windows`, one of them, fixed at startup.
3. one per open overlay, bottom-to-top, mirroring the compositor's own stack.

Lookup walks that list **from the top down** and takes the first context that binds the
chord. That one sentence replaces four separate rules: which surface owns a key, whether a
shortcut fires over an open picker, what a platform-specific binding is, and what
precedence means.

*Why a stack of names and not a predicate language.* Zed evaluates context predicates
(`&&`, `!`, `>` for ancestry, `os == macos`) against a focus chain; VS Code evaluates
`when` clauses over a registry of global context keys; Sublime evaluates a `context` array
of key/operator/operand tests. All three need an expression language because many regions
can hold focus at once and their conditions do not nest. Vortex's focus chain *is* the
compositor stack - already an ordered list, and two deep. A stack of names is the same
power here for none of the machinery; it is Emacs' model rather than a thinned-out Zed.
**Trigger to revisit:** the first binding whose condition is not "which surface is on top"
- a language filter, a read-only buffer, a debugger state. Contexts grow a predicate then,
and this paragraph is the record of why they did not have one before.

*Why it is urgent rather than nice.* Helix has this compositor shape and these hardcoded
surface keys, and has not been able to undo it: the request is open since
[#615](https://github.com/helix-editor/helix/issues/615), with
[#5505](https://github.com/helix-editor/helix/issues/5505) (remapping for any component)
and [#3205](https://github.com/helix-editor/helix/issues/3205) (bindings for prompts)
behind it, because the fix needs an architecture change to its component layer. That
change is cheap at five surfaces and expensive at fifteen.

The contexts are a **closed set** - an unknown name in a config file is an error, not a
silent no-op, the rule §10.5 already applies to chords and commands:

| context | raised by | an unbound printable key is |
|---|---|---|
| `editor` | always | text (inserted) |
| `macos` / `linux` / `windows` | the platform | - |
| `picker` | `Picker` (file, buffer, theme, encoding, line-ending, palette, global search) | text (the query) |
| `prompt` | `Prompt` (save-as) | text (the input) |
| `find` | `Find` (the find / replace prompt) | text (the focused field) |
| `confirm` | `Confirm` (close guard, external-change reload, overwrite) | a decline |
| `replace` | `QueryReplace` (the y/n/a/q walk) | the end of the walk |

One `picker` context for all seven pickers, not one each: nothing today would bind them
differently. **Trigger:** the first binding that should differ between two pickers.

**Command names.** One namespace still - `Command::parse` answers for all of them, so the
config's forward lookup and the palette's reverse lookup stay the two functions they are.
What is new is that **a command declares which contexts it is valid in**, and a binding
outside that scope is a `KeymapError::WrongContext`, reported like a bad chord. Without
it, `next_item` written in `[keys]` would be a row that parses, applies, and never fires -
the silent failure §8 forbids. Shared names where the meaning is shared, specific names
where it is not:

- `accept` - commit the surface (`picker`, `prompt`, `find`).
- `cancel` - dismiss it (`picker`, `prompt`, `find`, `confirm`, `replace`).
- `delete_backward` - already the editor's name for it, and scoped to `picker`, `prompt`
  and `find` as well: it is the same intent over whatever field has focus.
- `next_item` / `previous_item` - move the highlight (`picker`).
- `next_field` - the replace prompt's second field (`find`).
- `confirm_yes` / `confirm_no` (`confirm`).
- `replace_yes` / `replace_no` / `replace_all` / `replace_quit` (`replace`).
- `nop` - every context.

They become a public contract the moment `keys.toml` ships, which is why they are settled
here rather than after the fact (§10.5's own rule about command names). None of them joins
the **palette**: its list is curated by hand rather than derived from the vocabulary, and a
surface command is unrunnable from the one place its surface is not open. The scope a
command declares is what makes that a rule instead of an oversight - the palette lists
editor-scoped commands, and `shortcut_for` is asked in the editor context to fill its
shortcut column.

**What a layer does with a key it was not given.** The compositor resolves the chord in
the layer's context and hands the layer *both* the key and whatever binding it found
(`handle_key(key, bound: Option<Command>)` - one method rather than two, so the layer
still owns the decision). A miss is not one behavior but three, and each is a property of
the surface rather than of the table:

- **take it as text** - the picker's query, the prompt's input, the find prompt's field;
- **treat it as a decline** - `Confirm` answers no to every key but `y`, and
  `QueryReplace` ends the walk on anything that is not `y`/`n`/`a`. Neither is expressible
  as a binding ("bind everything else"), and both are what makes a mistyped key harmless;
- **defer it** - fall through to the next context down.

**Which of the three applies is decided by one test per policy, not by the surface's
mood.** A *text* surface asks `is this key text?` (`text_key`) and defers everything
else - that is what the table's last column is, and what keeps the guarantee below
true: an unbound Esc is not printable, so it falls through. A *decline* surface asks
the narrower `is this somebody's shortcut?` (`is_command_chord`) and declines
everything else, because a question is modal over the keyboard: only the keys that were
never its to answer may leave it. Ctrl+S over a confirmation therefore still saves,
while Backspace answers "no" instead of reaching the buffer behind the question.

Reading the decline policy as "decline if *printable*" - the same test the text
surfaces use - is wrong twice over, and both were found by review rather than by the
suite. Backspace at "discard your changes?" would delete a character behind the
question and dismiss it unanswered. Worse, a confirmation is raised by an async
notification onto whatever is already open, so it can sit over a live picker: a
deferred Enter is offered to the layer below, resolved in *that* layer's context, where
it is `accept` - and the picker the user cannot see commits its highlighted row.

This is what replaces the `if key.modifiers.contains(CONTROL) { return Ignored }` guard
copied into five layers today: a Ctrl chord reaches the editor because nothing above it
bound the chord, not because it is a Ctrl chord. Two §11 debt entries are paid off here -
the one wanting an explicit `EventResult::Deferred`, and the one about that convention
being split across two files.

**No surface can be locked shut by a config.** Unbind `cancel` in `[keys.picker]` and Esc
is unbound *there*; it is not printable, so it is not taken as text, so it falls through
to the editor context - where it is `collapse_selections`, which fires and dismisses the
overlay. The way out survives a bad config by construction, not by a special case.

**Config shape.** `[keys]` stays the editor table it already is; a *sub*table is a context:

```toml
[keys]                 # the editor context
"ctrl+e" = "quit"
"ctrl+f" = "nop"       # free the chord

[keys.macos]           # only on macOS
"ctrl+c" = "quit"

[keys.picker]
"ctrl+n" = "next_item"
```

`[keys]` is the editor context rather than `[keys.editor]` being required: one way to say
it, and the common case stays a line shorter. A value is either a string (a binding) or a
table (a context); contexts do not nest, so a table inside a table is an error. The rest
is the rule already in force - later rows win, the file layers over the built-ins per
context, and one bad row costs one binding and is reported.

**No chord reaches the screen as a literal.** A binding the user can change is a binding
the editor must not hard-code into the text it shows. `shortcut_for` grows a context
argument (the palette asks the editor's) and becomes the *only* way a key name is
rendered anywhere.

This is not a tidiness rule; the drift has already happened. `--help` advertises
`Ctrl+F  Search the project`, and has since M7 moved project search to Ctrl+Shift+F and
gave Ctrl+F to the in-buffer find. Nothing failed, because nothing connects the sentence
to the binding. Every site below is the same hazard, and rebinding multiplies it - a user
who moves a key today gets an editor that confidently names the old one.

The full inventory of places a chord is shown, and what each becomes:

| site | today | after |
|---|---|---|
| the palette's shortcut column | `shortcut_for` | unchanged - it is the model for the rest |
| the query-replace question (`(y)es (n)o (a)ll (q)uit`) | literal | rendered from the `replace` bindings |
| the four confirmations (`(y/N)`) | literal | rendered from the `confirm` bindings |
| `--help`'s key list | literal, plus a `cfg!` twin for the OS-conditional lines | label + `shortcut_for`, per row; the `cfg!` twin deleted, since `mod` resolves per OS |
| the README's key table | literal | still literal - a document cannot query a keymap - but **held to the default keymap by a test**, the device the theme default already uses (`Theme::default()` against `undertow.toml`, and the README's own config example against the real parser) |

The help's list and the palette's list stay **two** curated lists, not one: they carry
different labels for different audiences (the help explains, the palette names), and only
the *chord* is generated. The rule is "no literal chord", not "one list". A help row whose
command has no binding is **omitted** rather than printed blank, so unbinding something
removes it from the help instead of advertising a key that does nothing.

**What is deliberately not data**, each with the condition that would change it:

- **The text-entry fallback and the three unbound-key policies above.** They are what a
  table cannot express.
- **Mouse gestures.** A drag has no chord. *Trigger:* a second pointer convention worth
  choosing between.
- **Key *sequences*.** Unchanged from §11 - trie, pending-prefix state, config grammar,
  `shortcut_for` rendering, then the which-key popup, in that order. Contexts do not need
  sequences and sequences do not need contexts, so neither blocks the other. *Trigger:*
  unchanged - modal editing or a leader key.
- **Command arguments.** Zed passes `["workspace::ActivatePane", 0]` and VS Code an `args`
  object; Vortex names `insert_tab` and `insert_newline` separately instead. Arguments
  would end `Command` as a data-free `Copy` enum, which is exactly what makes
  `shortcut_for`'s identity match exact and page-free (§10.5). *Trigger:* the first
  command whose useful values do not fit a handful of names.
- **Command *lists*** (Helix's `"ret" = ["open_below", "normal_mode"]`). *Trigger:* a
  macro facility, or a user wanting to compose two commands.
- **Per-platform *surface* bindings.** The active list is flat and the platform sits below
  the layers, so it cannot qualify one. *Trigger:* the first surface binding that must
  differ per OS.
- **Runtime rebinding.** A theme is swappable mid-session because previewing one is the
  point; a keymap is not. The config file stays its only writer.

#### Theme files (built)

Styling is the first configuration surface to become a real file, so it settles the
format the rest of the config will use.

- **Format: TOML**, one key per style slot of the frontend's `Theme`, each an inline
  table: `selection = { fg = "#eef1fa", bg = "#2b3557", bold = true }`. Attributes are
  `bold`/`dim`/`italic`/`underlined`/`reversed`.
- **Colors are `#rrggbb` only.** A *named* ANSI color is remapped by the user's terminal
  profile, so a theme built from one cannot promise the contrast it was designed with.
  Hex is 24-bit and literal; the 256/16-color fallback is the resolution step above, not
  the file.
- **Every slot is optional**, inheriting the built-in default, so a user theme can be
  three lines that recolor one thing. A slot that *is* present replaces its default
  wholesale rather than merging into it - `current_line = { bg = … }` has no foreground.
- **Unknown keys are an error**, not a silent no-op: a typo must be reported, or the user
  stares at a theme that "did not apply" with nothing to go on.
- **The file stem is the theme's name** (`undertow.toml` → `undertow`), so listing the
  available themes never parses them. Parsing is lazy - a broken file costs an error
  toast when it is picked, not a startup failure or a silent omission from the list.
- **Two locations, always both.** The shipped themes are compiled in with `include_str!`
  (a fresh install has themes with an empty config dir, and nothing is ever written to
  the user's disk unasked); `$XDG_CONFIG_HOME/vortex/themes/*.toml` - else
  `~/.config/vortex/themes` - is scanned too, and a user file **shadows** a built-in of
  the same name. XDG on every platform, macOS included: that is the terminal-editor
  convention and it keeps a dotfiles repo portable.
- **The theme is swappable at runtime** (Ctrl+T, `open_theme_picker`), and the picker
  **previews**: moving the highlight applies that theme immediately, Enter keeps it, Esc
  restores the one you opened with. This is only possible because chrome is entirely
  frontend-owned - a preview is a local repaint, and the seam never hears about it. It
  is also why `Layer::restyle` exists: layers cache their `Style`s at construction, so a
  theme change has to hand the new ones to every open overlay and to the toast surface.
- **Persisted by the config file, not by the picker.** A pick lasts the session;
  `theme = "…"` in `config.toml` is what makes it the theme you start in. The picker never
  writes to the user's disk - the same rule the themes directory follows.
- **The built-in `Theme::default()` is written in Rust, not parsed at startup**, so the
  editor can never fail to have a theme. A test holds it equal to `themes/undertow.toml`,
  which is what keeps the hand-written copy and the file a user reads from drifting.

**The loader (built, M5).** The config lives behind a single resolved `Config` value built
once at frontend startup (next to argv, before the first frame) and threaded into the
render/input paths - **not** scattered constants. Because every call site already read from
that value while it was still the built-in `Default`, adding the file touched only the one
construction point, exactly as this section predicted. `config.toml` sits beside the themes
directory (`$XDG_CONFIG_HOME/vortex`, else `~/.config/vortex`), `--config <path>` rides the
same argv parser, every key is optional, and an unknown key is an error - the theme files'
rule, for the same reason. A config that will not load never stops the editor starting: it
comes up on defaults and reports the problem as a toast, since the config resolves before
the terminal is even in raw mode.

**The one thing that crosses the seam.** Most settings are frontend-owned by nature -
colors, key bindings, tab width (a tab is one byte whatever it is painted as). The
final-newline policy (§10.1) is not: the *core* writes the file. It travels as
`Action::Configure(CoreOptions)` rather than a constructor argument, because a remote
frontend reads its user's config on its own machine and has to be able to send it, and
because that leaves one path whether a setting arrives at startup or changes mid-session.
`CoreOptions` stays deliberately small - a setting belongs in it only if the core is what
acts on it, and the next candidate is §10.4's degradation threshold.

---

## 11. Deferred (not silently skipped)

- **Full CRDT / replica model** - only when remote or collaboration is real. Anchor API is
  kept swap-ready (§2.1); not built now.
- **Out-of-process RPC** - the channel is the seam; add the wire when a non-Rust or remote
  frontend exists (§1). The transport ships the `Delta` stream the core already produces
  (§5) - no snapshot reconstruction needed.
- **Custom cell renderer** - a double-buffered dirty-rect renderer *replacing* ratatui's
  (Helix compositor's job 1, §7.5); earn it by outgrowing ratatui's cell-diffing (§7). This
  is **not** the overlay/layer compositor (job 2), which *is* built (§7.5) - only the renderer
  underneath it stays deferred.
- **Crash-recovery backups** - room left in the buffer module (§8); not v1.
- **Tier-3 huge-file backend (bigger-than-RAM editing)** - a paged / mmap piece-table
  buffer (§10.4). Kept swap-ready by putting the buffer behind a `Buffer` trait (§2.1) so
  `crop` never leaks into the core's public surface. **Deliberately not built**, because it
  collides with the §5 render model: the zero-copy `ViewSnapshot { text: Arc<Rope> }` trick
  only works for a fully-in-memory persistent structure. An mmap/paged buffer cannot be
  `Arc`-cloned into a cheap immutable snapshot, so Tier 3 would force snapshots to ship
  viewport *slices* (the same incremental-diff work already deferred to `proto/` for
  remote). You can have cheap zero-copy snapshots *or* bigger-than-RAM editing cleanly, not
  both for free - we choose the former. If bigger-than-RAM editing ever becomes a real
  goal, it is a buffer + §5 redesign, not a bolt-on.

### Acknowledged subsystems (scoped, not forgotten)

These are real editor features with real complexity. Named here so their absence from the
early milestones is a deliberate scope choice, not an oversight:

- **Clipboard / yank-paste.** Core owns register/clipboard *state*; the frontend bridges to
  the OS clipboard. Must include **OSC 52** (clipboard over the terminal) so copy/paste
  works over SSH - directly relevant to the remote-frontend future (§0). Target: M1-M3
  band.
- **Search + regex.** *Built (M7), in both halves.* **Cross-file** search is a frontend
  subsystem (`vortex-tui::search`) behind the global-search picker: `regex` + `ignore` on a
  worker thread, searching the files on disk. **In-buffer** search is a *core* one
  (`vortex-core::search`), because its answers are selections over the rope the core owns -
  `regex` joined that crate's dependencies as this section said it would, so one engine
  serves both callers and the two searches cannot disagree about what a pattern means. The
  jobs stay deliberately different: one finds a place to go, the other builds a selection
  set. Still absent: `split-on-regex` (§12.2), the one selection operation of the three
  that has no UI asking for it yet.
- **Keymap configuration.** Built (M5): a `[keys]` table in `config.toml` is layered over
  the built-in bindings through the same `Chord`/`Command` string format the defaults are
  written in. What remains is the richer *modal* design - chord sequences, per-mode maps,
  modal vs modeless - drafted alongside §12.2's `Action` vocabulary. Target: M1+.
- **Chord sequences + the which-key popup.** *Cut from M7 (2026-07-27), together, because
  they are one feature and not two:* the popup is a view of a pending prefix, and a flat
  keymap has no such thing (§7.5, §10.5). The order is fixed - trie keymap, pending-prefix
  state, config syntax, `shortcut_for` rendering, *then* the popup, which is the small part.
  **Trigger: the first thing that genuinely wants a prefix** - modal editing, a leader key,
  or a command surface that outgrows one chord each. Building it for the popup alone would
  buy a worse command palette at the cost of changing the keymap's public contract, so the
  editor stays modeless-with-a-palette until something asks otherwise.

### Known structural debt (identified 2026-07-21, with triggers)

Cleanups that were identified, judged real, and deliberately not taken yet. Each names
the condition that should pull it forward, so they are scheduled rather than forgotten.
None is a correctness bug today.

- ~~**The head-bar / body / status split is written out in four places.**~~ **Paid off
  (2026-07-30), by its own trigger.** The note predicted that sticky context - "on M8's own
  list" - would be the change that made the copies disagree with no compile error and no
  failing test, and it was: the header pushes the text down, so `on_scrollbar`,
  `pointer_offset`, the scrollbar's drag handler and the new pinned-row hit test would each
  have had to subtract the same two numbers by hand, and a fourth copy was being added at
  the same time. They now all ask `layout::row_at(screen_row, header_height) -> Row`
  (`Head` / `Header(i)` / `Text(i)`), which is a pure function with its own tests rather
  than the `body_rect(area) -> Rect` this note proposed - the callers want *which row is
  this*, not a rectangle, and answering that directly kept `page_height` out of it. The
  status bar stays outside the split deliberately: it is a row the event loop answers
  before any of these, so distinguishing it would mean carrying a screen height into a
  question none of them otherwise needs it for.
- **A guide is a glyph substitution outside `render_line` plus a style overlay inside it.**
  Every other marker (ruler, selection, search, syntax, diagnostic, secondary caret) is one
  `(Range<usize>, Style)` in the overlay list; the indent guide alone needs a second
  mechanism, because an overlay can restyle a cell but not rewrite it. Widening the tuple
  to carry an optional replacement glyph would let `render_line` emit the character where
  it already resolves the style, and delete the per-row `String`. **Deferred because**
  there is exactly one consumer, and none of the chrome still queued would be a second
  one: git signs paint in the *gutter*, sticky context is its own pinned widget, and the
  completion popup's ghost preview is `VirtualText`, which **inserts** rather than
  substituting a same-width cell. Widening the frontend's one intra-line styling seam for
  a single caller is the premature abstraction CLAUDE.md rules out. **Trigger: a second
  marker that replaces a cell it did not widen** - whitespace visualization (`·` for
  spaces, `→` for tabs) is the obvious candidate, and it would want exactly this seam.
- **The scrollbar's drag state is a bare `bool`, now in two places.** The body's lives in
  `event_loop`: press sets it from the hit test, a drag inherits it, a release and a
  `scrollbar = false` clear it - which no test drives, so it is covered only by the pty
  run recorded in §14. The picker's bar repeats the shape as a `dragging` field on the
  layer, and *that* one is tested, because the layer owns its own geometry and takes
  synthetic events. The two are not merged and should not be: `Layer::handle_mouse`
  defaults to `Consumed` and the overlay arm returns before the body's hit tests run, so
  an open picker already holds the pointer exclusively and the event loop's latch is
  unreachable while one is up. What the latch answers - "did this gesture start on *my*
  bar" - is only answerable by whoever owns the geometry, which under the trait's
  no-stored-geometry contract is the layer. **Deferred because** `click::Clicks` earned
  its module by needing a clock injected; a `bool` set on press and cleared on release
  has no logic a struct would make clearer. **Trigger: a third stateful gesture** (a fold
  drag, a split resize) - at two, the coordination between them is still readable in one
  screen.
- **Quit is detected by sniffing the `Action` value in the frontend.** `dispatch_command`
  compares against `Action::Quit` and exits the loop immediately after sending it, while
  the core's own `Notification::ShuttingDown` (which exists for exactly this) is drained
  and ignored - two shutdown signals that must agree. Exiting on the notification, or on
  channel closure, would delete the special case. **Deferred because** the right shape is
  decided by core-side "unsaved changes, really quit?": if the core can *refuse* a quit,
  the frontend needs that response path anyway, so building the seam first means guessing
  at it. **Trigger:** the confirm-on-quit feature. A smaller independent piece can land
  sooner - the frontend currently keeps painting a stale snapshot if the core thread dies
  while the user is idle, since only a later keystroke notices the closed channel.
- **Paste modality is still hardcoded in the event loop.** *Half resolved:* the mouse
  now routes through the compositor exactly as keys do (`Layer::handle_mouse`, defaulting
  to `Consumed` because an overlay is modal over the screen as well as the keyboard), so
  clicking inside a picker, a prompt, or a toast is per-layer behavior rather than a match
  guard. `Event::Paste` is still swallowed by `if !overlays.is_empty()`, so input routing
  remains at two altitudes for that one kind. Widening the seam to whole events
  (`handle_event`, with defaults) would finish it. **Trigger:** pasting into the prompt -
  which is also what the prompt needs before a click can position its caret, since the
  caret only ever sits at the end of its input today.
- **~~"A shortcut fires over a picker" is a convention split across two files.~~ Paid off
  by M9 Stage 2**, as that entry predicted it would be - as a side effect rather than as
  its own change. The `Ignored`-for-any-Ctrl-chord guard is deleted from all five layers:
  a layer is handed the binding its own context resolved (`handle_key(key, bound)`), so a
  chord falls through because nothing above bound it, not because it carries Ctrl. The
  rule is in the seam now, and a new layer type inherits it by declaring a context.
  **What is left of it** is the other half the entry named: `overlays.dismiss()` still
  clears the *whole* stack when a fall-through key turns out to be bound, which is the
  wrong scope for nested overlays. A named `EventResult::Deferred` was not needed for the
  first half and would not fix this one either - the fix is the event loop popping only
  the layers that declined. **Trigger:** the first nesting a user can actually reach that
  is not "everything closes anyway", and the theme-picker entry below, which wants the
  same widening of the `Layer` seam.
- **A shortcut fired over the theme picker keeps the preview.** `Compositor::dismiss`
  drops the stack without asking the layers, so the "Esc restores what you opened with"
  contract is only honored by Esc: pressing Ctrl+S while previewing leaves the previewed
  theme applied. Defensible as "you saw it and moved on", and the alternative (running a
  layer's cancel command on dismissal) is the same widening of the `Layer` seam the
  `EventResult::Deferred` item above already wants. **Trigger:** taking that item - the
  two should be designed together, not one at a time.
- **Cursor motion allocates the caret's line on every keystroke.** `Text::line` returns an
  owned `String`, so Left/Right/Backspace/Delete each copy the current line and vertical
  motions copy two, per cursor. Bounded by line length as §10.4 promises, but on a file
  whose content is one very long line (minified JS/JSON) holding an arrow key copies
  megabytes per repeat. A borrowing accessor (grapheme iteration over the line's chunks,
  or a `Cow` that borrows when the line lies in one chunk) fixes it additively, without
  putting `crop` in the public API. **Deferred because** it is an additive `Text` method,
  equally cheap to add later, and it deserves a benchmark rather than being folded into a
  cleanup pass. **Trigger:** a benchmark harness, or the first large-file report.
- **The file picker reads the filesystem on the UI thread.** Ctrl+O runs a
  synchronous recursive `read_dir` of up to `MAX_FILES` entries before the overlay paints,
  so a cold or network filesystem freezes the editor until it finishes. The cap bounds the
  match cost, not the walk latency. The preview pane then adds a second synchronous read -
  bounded at `PREVIEW_BYTES` and only when the highlight *moves*, so it is a fixed cost per
  keystroke rather than an unbounded one, but on a remote tree it is a stall per arrow key.
  Walking (and previewing) on a background thread and feeding results in incrementally suits
  the compositor's per-tick repaint already. **Trigger:** the picker being used on a large or
  remote tree, or the same background-work machinery arriving for §2.3's off-thread file
  loads. (The global-search picker went the other way and put its walk on a worker thread
  from the start, feeding the list through `Layer::tick` - the machinery this entry wants
  now exists next door.)
- **The search jump's guard infers its target from the *active* buffer.**
  `PlaceCursorAt { in_file }` decides whether to place the caret by comparing the named
  file against `session.active()`'s path - a proxy for "did the `Open` this jump follows
  actually land". The equivalence holds only because every successful open activates what
  it opened, which nothing in the type system enforces. A combined `Action::OpenAt` would
  thread the buffer the open *produced* straight to the placement instead of re-deriving
  it from ambient state. **Deferred because** `PlaceCursorAt` has to stay a standalone
  action anyway (a `:42` goto uses it with `in_file: None`), the guard reuses the
  `file_identity` comparison `Open` already makes, and with one window there is no way for
  the proxy to be wrong. **Trigger:** any feature that opens a buffer *without* focusing it
  - a split, a background open, "open all N search results" - at which point the guard
  starts silently dropping legitimate jumps.
- ~~**Global search restarts on every keystroke, with no debounce.**~~ *Taken.* A query
  now waits 150ms (the `HIGHLIGHT_WAIT` shape `main.rs` already uses) before it is walked,
  so typing `needle` starts one search rather than six, and the intermediate prefixes are
  never walked at all. The clock this entry said the picker lacked is the render tick it
  already had: `ItemSource::take` is called once per tick, so the source starts the walk
  there rather than on the keystroke. Two consequences worth naming. A bad pattern is now
  reported at the deadline rather than at the keystroke, which is the better behavior and
  not just a side effect - typing `(foo)` passes through `(`, and flashing that error on
  the way is noise about a pattern nobody had finished writing. And `Layer::tick` had to
  learn that a source can change what it *says* without producing a row: it now repaints
  on a changed `ItemSource::status` too, or the line under the query would keep saying
  `searching…` after the search had already failed to compile.

---

## 12. OPEN decisions

### 12.1 Extensibility engine (highest-leverage remaining choice)
Plugins ride the same message boundary (§1), so *when* we commit shapes the `Action`
vocabulary. Real trade-off, no default:
- **Lua (`mlua`)** - Neovim's path. Fast, familiar, biggest ecosystem gravity. Best
  velocity.
- **WASM (`wasmtime`/`extism`)** - Zed's path. Sandboxed, any language, safest.
  Best future-proofing, heaviest.
- **Steel / Rhai** - Rust-native (Helix → Steel/Scheme). Tightest integration, smallest
  ecosystem.

Decision pending.

### 12.2 The `Action` / `Delta` / `ViewSnapshot` / `Notification` vocabulary (owner: user)
The design surface where domain intent matters more than any library default - left for
the project owner to shape. Firm rule from §1:

> Model `Action` on **intent** (`MoveCursorWordRight`), not **keystrokes** (`Ctrl+Right`).
> Key→intent translation is frontend-owned; a future GUI has different keys, same intents.

Seed categories to draft:
- **Motion** (grapheme / word / line / paragraph / buffer-edge; `extend` variant for each
  to grow selections).
- **Edit** (insert, delete, replace, indent) - all map over the `SelectionSet`.
- **Selection** (add cursor, collapse to primary, select-all-matches, split-on-regex).
- **History** (undo, redo, jump to node).
- **View intent** the core must know (which buffer/region is focused, for lazy
  syntax/LSP) - kept minimal so the frontend still owns the literal viewport.
- **File/buffer lifecycle** (open, save, save-as, reload, close, conflict-resolution
  choice).
- **UI-commit** intents the frontend surfaces raise only on *commit* (the seam rule, §7.5):
  submit/cancel a prompt, run a picked command, accept a completion. Surface *navigation*
  (moving a selection, typing a filter) is never in the vocabulary - it stays frontend-local.
  The command palette is a discovery surface over the **same stable command identifiers the
  keymap binds** (§10.5), not a second vocabulary.

---

## 13. Test strategy

The headless, message-driven core (§1) makes this concrete:

- **Golden/interaction tests:** feed an `Action` script, assert on the emitted
  `ViewSnapshot`/`Notification` sequence. No terminal, no PTY, no snapshot-image
  fragility. This is the primary suite and covers the entire editing model. **Assert on
  projections, not whole snapshots** - check text + resolved selection positions +
  notifications, but not the raw `decorations` set, which shifts with tree-sitter grammar
  versions and would make tests brittle. Decoration *correctness* is covered separately with
  pinned grammar fixtures.
- **Property / state-machine tests** (`proptest`): generate random `Action` sequences and
  assert the model invariants hold - this is where editor bugs actually live (the
  *interaction* of edits, not any single function), and it catches what 100% line coverage
  cannot. Invariants:
  - Anchors survive arbitrary random edit sequences (position after edit == position
    computed by replaying).
  - `SelectionSet` invariant holds (always disjoint + sorted) after random motions/edits.
  - Undo tree: any edit sequence fully undoes to the initial buffer.
  - **Delta/snapshot agreement** (§5): applying the emitted delta stream from version N to
    a version-N buffer reproduces the version-(N+1) snapshot's text exactly. This guards the
    core invariant that the two seam outputs never diverge.
  - One `Action` over an N-cursor `SelectionSet` produces exactly one undo unit (§2.4).
- **Coordinate round-trip tests:** byte ↔ grapheme ↔ line/col ↔ UTF-16 conversions
  round-trip on adversarial input (CJK, emoji ZWJ sequences, combining marks, tabs, CRLF).
- **Encoding/line-ending fixtures:** load/save preserves original encoding + EOL on a set
  of fixture files.
- **Regression:** every bug fix adds a failing-first test (per CLAUDE.md).

### Coverage policy (max coverage, every turn)

Coverage is measured and **gated on every change**, not just at milestones - it is part of
the verification loop in `CLAUDE.md`, so a change that drops coverage does not pass.

- **Tool:** `cargo-llvm-cov` (LLVM source-based coverage; cross-platform, the current Rust
  standard). Settled: the gate is **`--fail-under-file-lines`** (per *file*, ≥0.8.6), not the
  package aggregate - a per-file floor stops one file slipping while a 100% neighbour masks
  it in the total. Branch coverage (`--branch`) may require nightly and is not relied on.
- **Ratchet, not a fixed number.** The gate is "coverage must not decrease" plus a floor.
  This encodes "max at each turn" without forcing tests on trivial glue. New code lands
  with its tests in the same change, so the ratchet only ever climbs.
- **Asymmetric floors, because the architecture is asymmetric:**
  - **`vortex-core` ≥ 90% lines** (target higher). The core is headless and message-driven
    (§1) specifically so it is almost fully testable via `Action`→`ViewSnapshot` scripts -
    there is no excuse for low core coverage. **One documented exemption:**
    `lsp/client.rs` (M2), the LSP subprocess + protocol shell - the one genuinely
    I/O-bound file in the core, the same shape as `vortex-tui`'s `main.rs`. Its decisions
    are extracted into pure functions (`check_encoding`, `outgoing`, `initialize_params`)
    and covered 100%; the `run` loop needs a live language server, exercised by the
    `--ignored` `lsp_rust_analyzer` integration test. The gate passes it via
    `--ignore-filename-regex 'lsp/client\.rs'` rather than gaming the number.
  - **`vortex-tui` ≥ 60% lines.** Terminal I/O, raw-mode setup, and the render loop are
    hard to cover meaningfully; logic that *can* be extracted from the frontend (keymap
    resolution, viewport math, display-column layout) is pulled into testable functions and
    covered, while the thin I/O shell is not chased for percentage.
- **Coverage is a floor, not the goal.** Line coverage proves a line *executed*, not that
  it is *correct*. The property tests and interaction tests above are the real correctness
  bar; coverage guards against untested code sneaking in, nothing more.
- **Exclusions are explicit.** Genuinely untestable glue (terminal escape I/O, `main`
  wiring) is marked with coverage-ignore annotations *with a reason*, never silently
  dropped - so the number reflects reality instead of being gamed.

---

## 14. Milestones (each proves part of the architecture)

Incremental build order so the risky assumptions are validated early, not at the end.

- **M0 - Workspace skeleton.** Cargo workspace; `vortex-core` with `Action`/`ViewSnapshot`/
  `Notification` enums (stubs) + single-owner actor loop; `vortex-tui` that connects, sends
  a quit `Action`, prints a snapshot. Proves the seam compiles and the boundary holds
  (core has no terminal deps). *Verify:* CLAUDE.md loop is green against a real build.
- **M1 - Edit + render.** `crop` buffer + `SelectionSet` (single selection first) +
  insert/delete/motion `Action`s; core emits `Delta` + derived `ViewSnapshot` (§5), both
  `serde`-serializable from the start (§8.1); `vortex-tui` renders from the snapshot with
  sync-output framing and Kitty input. Proves §5 render model end-to-end. *Verify:* type in
  a real terminal, no tearing, cursor by grapheme; delta/snapshot-agreement property test
  (§13) passes.
- **M2 - Async runtime + LSP smoke. DONE.** smol executor; `async-lsp` spawns a real
  server (e.g. `rust-analyzer`), completes `initialize`, receives diagnostics, maps their
  UTF-16 positions correctly, and surfaces them as `Underline` + `GutterMark` decorations
  (§5) - the first non-syntax producer on the decoration channel. **Validated the one
  unproven stack assumption (§3):** smol + `async-lsp` + real `rust-analyzer` runs with no
  tokio in the tree and negotiates UTF-16. The decoration channel (`decoration.rs`) landed
  here as §5 specifies - one anchor-backed set, resolved for the visible range - so M4's
  syntax highlights are an additive `Highlight` variant, not a new channel. *Verified:*
  the `lsp_rust_analyzer` integration test underlines exactly `msg` (the UTF-16 column
  where byte/char/UTF-16 all disagree), and the real binary renders underlines on the
  flagged tokens in a terminal. **Deferred within M2:** incremental `didChange` (full-text
  sync ships now - it cannot desync, §5 note in `lsp/mod.rs`); preferring a server's UTF-8
  encoding (UTF-16 is advertised alone, one conversion path); completion / hover / goto
  (the client sends only `didOpen`/`didChange` and consumes `publishDiagnostics`); a
  per-file project-root walk (roots at the cwd); surfacing an LSP spawn failure as a toast
  (it degrades silently to no diagnostics).
- **M3 - Anchors + undo tree + multi-cursor.** Full `SelectionSet`, anchor layer,
  coalesced undo tree. *Verify:* property tests (§13) pass; multi-cursor edit + undo works
  in-terminal.
- **M4 - Syntax highlighting. DONE.** tree-sitter background reparse on snapshots feeding a
  new `Highlight` decoration on the M2 channel (§5) - the additive variant that decision
  promised, landing without touching M2's producer code. The highlighter is a second
  decoration producer shaped exactly like the LSP client (`vortex-core::syntax`): full-text
  in, highlight spans out, spawned by the frontend on its own thread, off the keystroke
  path. **Full reparse per snapshot, not incremental** - the same "cannot desync" call M2
  made for `didChange` (incremental deferred, §14). The core emits a fixed semantic
  `HighlightKind`; the theme maps it to a color (8 syntax roles per theme), so tree-sitter's
  own types never cross the seam - the discipline that keeps `lsp_types` out of core too.
  **Genuinely dynamic grammars:** a grammar is a `cdylib` (`grammar-rust`) the frontend
  `dlopen`s at runtime from the runtime dir via `libloading`, exporting a uniform
  `vortex_grammar` entry point - never a compile-time dependency of the editor, so adding a
  language is a new grammar crate plus a `grammar_target` row (§3, §14). Selection was
  reordered *under* highlights so selected code keeps its syntax colors on the selection
  ground. *Verified:* the core unit-tests pin exact spans against a real grammar in-process
  (no exemption - unlike `lsp/client.rs`, the engine is pure CPU); the real `vortex` binary
  in a PTY paints `fn`/`let`, `"strings"`, `// comments`, and `i32` in their theme colors.
  **Deferred within M4:** incremental `didChange`-style reparse (full reparse ships now - it
  cannot desync); language injection (embedded languages / doc-comment code - the injection
  callback returns `None`); parse cancellation; an interval index for `highlights_in` /
  `transform_through` (linear now, the case the decoration channel's own comments flag).
- **M5 - File handling hardening. DONE.** The buffer now always holds UTF-8 with LF
  terminators - one shape for every motion, column and edit rule - and a per-document
  `FileFormat` remembers what the file on disk actually is, so a save reproduces it.
  Detection is sampled as §10.4 requires: BOM, then whether a bounded prefix is valid
  UTF-8, then windows-1252, which maps all 256 bytes and so keeps a file in an encoding
  we cannot *name* byte-exact rather than refusing to open it (a proptest pins that round
  trip over arbitrary input). Two refusals come with it, both because the alternative
  loses data silently: a save whose text does not fit the file's encoding fails and leaves
  the buffer dirty rather than letting `encoding_rs` write `&#128512;` into a source file,
  and a file with NUL bytes is not opened at all, since the windows-1252 fallback would
  otherwise present a PNG as mojibake that corrupts on the first edit.
  **Read-only is enforced by the core** (§10.3), for two reasons decided at load: the file
  cannot be written - probed by opening it for append, because permission *bits* are not
  the question a mode-644 file owned by someone else answers - or it did not fully decode,
  where saving would write U+FFFD over bytes the user never touched. The guard sits on
  `Step`, which every text change funnels through, so a new step has to state which side it
  is on rather than defaulting to "allowed". A FIFO or device is refused before the read,
  because reading one blocks the actor thread forever.
  **External changes** (§10.2) arrive from a frontend-owned `notify` watcher over the same
  producer seam LSP and syntax use; the *policy* stays in the core, which reloads a clean
  buffer and refuses to choose for a modified one (`ExternalChange` → the frontend asks →
  a forced `Reload` comes back, the close guard's shape). Each document remembers the
  mtime+length it last accounted for, which is what tells the editor's own save apart from
  someone else's write - and one write raises one prompt however many events a platform
  sends. **Configuration is a file** (§10.5): `config.toml` with `theme`, `tab_width`,
  `final_newline` and a `[keys]` table layered over the built-in bindings, plus
  `--config PATH`. The settings the *core* acts on cross as `Action::Configure`, which is
  where §10.4's degradation threshold will go. *Verified:* fixture round-trips, a
  byte-preservation proptest, fault injection (unwritable files, FIFOs, directories,
  binaries, truncated UTF-16), and the whole arc driven in a real terminal.
  **Deferred within M5:** a statistical encoding detector (`chardetng`) that would *name*
  Shift-JIS and KOI8-R rather than preserving them as windows-1252 mojibake; loading large
  files off the actor thread (§2.3). The `set encoding` escape hatch has since landed as
  `Action::SetEncoding`/`SetLineEnding`, reached by clicking the status bar's own readout
  of them - the place that shows the guess is the place to correct it.
- **M6 - Frontend UI shell.** The overlay compositor (§7.5, job 2 only) + a message/toast
  surface (consuming `Notification`) + a prompt line. Proves the layer stack and the
  commit-only seam rule (§7.5): surface navigation stays frontend-local, only the committed
  intent becomes an `Action`. *Verify:* save-as via a prompt, an error toast from a failed
  save, and a stacked overlay that dismisses top-first - in a real terminal, with no core
  round-trip for navigation.
- **M7 - Pickers + palette + search. DONE.** *(Named "+ which-key" while it was open; the
  popup was cut at the end of the milestone rather than built - see the closing note below.)*
  `nucleo` fuzzy matching (§3 addition); file /
  global-search / buffer pickers with a preview pane; a command palette over the §10.5
  command identifiers; a which-key popup driven by the keymap. Multi-buffer was the one core
  change in this arc, since buffer-switching needs it: `Session` owns many `Document`s,
  `Open` switches to an already-open path, and `SwitchBuffer`/`CloseBuffer` join the
  vocabulary - with the close guard in the *core* so no frontend can discard unsaved work by
  forgetting to ask (§8). It also made two latent bugs live, both fixed with the arc: syntax
  batches now carry the buffer they were parsed against (versions are per-buffer, so a
  version-only guard painted the wrong buffer), and diagnostics route by path to whichever
  document owns them rather than only the active one.
  The **preview pane** followed, on the shared picker rather than the file picker: a picker
  arms it with a source that turns the highlighted item into lines, so a second picker that
  wants one supplies a closure instead of a layout. It fills only when the highlight *moves*
  (a repaint must not re-read a file) and is dropped below 80 columns, where two half-columns
  would be less readable than the list alone. The file picker's source is the one that
  exists, and it decodes through the core's own loader - a preview that guessed the encoding
  differently would misrepresent the thing it is there to identify, and it names a binary
  file rather than showing it, the same refusal `Open` makes (§10.3).
  The **global-search picker** closed the arc, and needed three things nothing before it
  had. A picker whose rows are not a list it was handed (`ItemSource`: the query starts a
  search rather than filtering, and results are appended in arrival order, never re-ranked,
  so a row cannot move out from under a click). A `Layer::tick` so those results appear
  while you wait rather than on the next keystroke - the first surface fed by something
  other than input. And `Action::PlaceCursorAt`, the one core change: a pick sends `Open`
  and then the position, both down the same channel, so the jump resolves against the
  buffer the open just produced instead of waiting for a snapshot to compute an offset
  from. The search itself is frontend-owned on a worker thread, the shape the LSP client
  and the highlighter already have - a grep is filesystem work, and the actor thread every
  keystroke goes through is the one place it must not run.
  A review of the arc turned up two defects in that jump, both from the same root: **a hit
  is only as fresh as the walk that found it**, and it is resolved against a buffer that
  has moved on. A column measured on disk can fall *inside* a character in an edited
  buffer, and placing that offset panicked the whole editor on the next conversion - so
  `Text::byte_of_position_clamped` now owns the rule that every position arriving over the
  seam rounds down to a boundary (`Buffer::byte_of_position`, its strict twin, refuses one
  instead; between them no conversion in the module can hand out an offset a later slice
  will panic on). And the open a jump follows can fail outright - the file deleted or
  replaced since the walk - which left the caret jumping to the hit's coordinates inside
  whatever buffer was focused; `PlaceCursorAt` now names the file it was measured against
  and is dropped when the active buffer is not holding it. The lesson is the general one
  for anything asynchronous crossing the seam (§2.1): a position that outlived its text
  must be *clamped and guarded*, never trusted.
  **In-buffer search** closed the other half of §11's search entry, and it is a *core*
  subsystem where the cross-file one is a frontend subsystem - the split follows from what
  each produces. A grep produces a place to go, which is filesystem work; a find produces
  *selections*, which are over the rope the core owns, so `select-all-matches` computed
  frontend-side would mean re-deriving the buffer's own text to hand back offsets into it.
  `vortex-core::search` is the whole of it: smart-case compile, line-bounded match
  iteration over a caller-supplied line range, a lazy next/previous with wrap, and capture
  expansion for replace. Four actions ride on it - `SelectNextMatch`, `SelectAllMatches`,
  `ReplaceMatch`, `ReplaceAllMatches` - and **the pattern rides on every one of them**
  rather than being remembered by the core, so a find-next key and a find-next after
  retyping are the same message and the core holds no search state that could disagree with
  what the frontend is showing. The frontend is what remembers, because it is what a
  find-next key has to ask.
  The decision worth recording is that **the live preview never crosses the seam**. Typing
  a query highlights every match on screen and scrolls the viewport to the one Enter would
  take you to, and none of that is an `Action`: the frontend holds the text (§5's snapshot
  rope) and owns the viewport (§5), so both halves of "show me what I would get" are
  answerable locally - which makes cancelling a search free, because nothing happened. Only
  Enter commits, and only then does the caret move. The preview compiles through the core's
  own `search::compile` rather than a second engine, since a preview that disagreed with
  the commit about what matches would be worse than no preview. The highlights are resolved
  **per frame over the visible lines only** and never cached: a cached set of byte ranges
  would paint over text an undo or a reload had moved, and the scan is viewport-bounded
  (§10.4), which is what makes recomputing it the cheap option rather than the careful one.
  Replace is a *walk*, not a chord: after the two-field prompt commits, a surface asks
  `y`/`n`/`a`/`q` about each match in turn. That is what a terminal can actually offer -
  replace-and-advance-while-the-prompt-is-open needs a modifier combination classic
  terminals cannot report (§9) - and the walk holds no match list, re-finding from wherever
  the caret ended up after each answer, so it can never be following positions an edit has
  moved (§2.1). Deliberately absent: a match **count** in the prompt, which would be a scan
  of the whole buffer per keystroke - the one thing §10.4 rules out off the viewport - to
  answer a question the highlights already answer.
  *Verify:* driven in a pty - typing a query previews and scrolls, `F3` walks the matches,
  a `y`/`n`/`y`/`q` walk edited exactly the matches agreed to, `a` replaced the rest with
  `$1`/`$2` capture expansion, and select-all-matches plus typing rewrote all five
  occurrences at once.
  **The which-key popup is cut from M7** (2026-07-27), and the milestone closes without it.
  The popup shows the continuations of a half-typed key sequence, so it presupposes a keymap
  that *has* sequences; this one is flat by design (§10.5 - one chord, modifiers and all,
  matched whole). Building it therefore meant building the sequence keymap first - a trie, a
  pending-prefix state in the event loop, config syntax for sequences, and sequence rendering
  in `shortcut_for` - with the popup as the last fifth of the work. That is a change to the
  keymap's *public contract* (§10.5: command names and chord grammar are a contract once a
  config file exists), and it should be paid for by wanting leader keys, not by wanting a
  popup. Stripped of sequences the popup degenerates into "list every binding", which is the
  question the command palette already answers - by name, with the shortcut beside each row.
  Deferred to §11 with that trigger: **if modal editing or leader sequences land, which-key
  arrives with them**; until then M7's answer to "what can I do here" is `Ctrl+P`.
  *Verify:* open a file via the picker, switch buffers, run a command via the palette -
  in-terminal.
- **M8 - Chrome + polish.** *(in progress.)* **Landed:** relative line numbers, rulers,
  indent guides, the scrollbar, sticky context. **Left:** git diff signs (a git-diff task
  feeding `GutterMark`s; its git source - `gix` / `git2` - is a §3 stack addition to
  raise), cursor-shape-per-mode. Unlike the earlier milestones these are
  **independent items sharing one shape** rather than an arc: each reads data the snapshot
  or decoration channel already carries, each is a config key over a theme slot (§7.5), and
  none needs a seam change - so they land one at a time and in any order, and the milestone
  is a list rather than a build sequence. Only the git signs have a prerequisite (the
  dependency choice above), which is why they are not first.
  **Relative line numbers** set the pattern the rest follow: a `LineNumbers` enum on the
  resolved `Config`, read per frame by the painter, with the decision recorded in §7.5 -
  no pure-relative mode, and the gutter width sized from the buffer in both modes so the
  text body never slides sideways. The runtime toggle mutates the live `Config` rather than
  a paint-only flag, which is what a theme pick already does (§10.5): the resolved value
  *is* the running setting, and the file only says what it starts as.
  *Verify:* each renders against real buffers / a real repo. Relative numbering driven in a
  pty - caret on line 5 of 9, the palette's toggle run, gutter repainting `4 3 2 1 _ 1 2 3
  4` with the caret's own row keeping its absolute `5`. Indent guides driven in a pty over
  a nested Rust file - guides at columns 0, 4 and 8 by depth, running through the blank
  line inside a block and stopping at the one trailing it, with the whole frame identical
  to the guides-off frame once the glyphs are replaced by spaces (the "nothing moves"
  claim, checked rather than argued), and the palette's toggle turning them on live. The
  scrollbar driven in a pty over a 60-line file in an 18-row body, by mouse: a press on
  the track's last row landing on the file's last line, a press near the top and two
  drags each landing on the offset the linear map predicts to the line, the second of
  those drags twenty columns *off* the bar and still scrolling rather than selecting
  text, and `Ln 1, Col 1` unmoved in the status bar throughout - the view goes where it
  is thrown and the caret stays where it was. Sticky context driven in a pty over a
  27-line nested Rust file in an 18-row body: two page-downs pinning `fn describe` /
  `for` / `if` / `} else {` - the innermost four of the six scopes enclosing the top row -
  with the text below them contiguous and the caret's own line still on screen, and a
  press on the second pinned row jumping to the line that opened it (`Ln 10`) and the
  header re-resolving to `mod` / `impl` / `fn` for the new top line. That run is what
  caught the follow bug the §7.5 entry records; the unit test that was supposed to cover
  it had asserted the wrong invariant and passed.

- **M9 - Every binding is data.** *(Done - see §10.5 "Every binding is data" for the full
  design and the reasoning behind each choice.)*
  M0-M8 externalized the
  *editor's* bindings and left the rest in code: five surfaces match key codes directly, a
  default binding cannot be removed, the platform split lives in `cfg!`, and the defaults
  are a Rust table no user can read. Two stages, in order, each shippable alone.
  **Stage 1 - DONE.** The three things every editor's keymap has and this one did not: a
  `mod` token for the platform command modifier, `nop` to unbind a chord, and the built-in
  bindings moved out of Rust into a compiled-in `keys.toml` (the Rust tables deleted, not
  mirrored). No new machinery, and each closes a gap a user meets on the first day. It also
  took the first half of "no chord reaches the screen as a literal": `--help`'s key list
  is rendered per row from `shortcut_for` (deleting its `cfg!` twin, which `mod` makes
  unnecessary) and the README's table is held to the default keymap by a test. That half
  fixed a live bug rather than preventing a future one - the help had advertised
  `Ctrl+F  Search the project` since M7 moved project search off that chord.
  Two details the design did not settle, decided while building it:
  - **One macOS-only binding survives the move, as a second file.** `mod` folds away the
    *command-modifier* split, but not `ctrl+c = quit`, which says "a chord that is free
    *because* of what `mod` resolved to" - true on a Mac, and on Linux a silent theft of
    copy's chord. It lived in `keys-macos.toml`, compiled in beside `keys.toml` and
    layered over it by one `cfg!`, which was the whole of what was left of the platform
    split - and is exactly what Stage 2's `[keys.macos]` absorbed, deleting the file. Written as a row in the
    same table instead - to be resolved by "later wins" - it would have depended on the
    `toml` crate handing back a *sorted* table, which is precisely the accident
    `extend_from_pairs` already refuses to rely on.
  - **`--help` renders against the resolved config, not the defaults**, so `Args::Help`
    carries the `--config` path and `-h` no longer returns from argument parsing on the
    spot (a `--config` written after it still has to be seen). A user who moves a chord is
    told the chord they moved it to, and a chord they `nop`'d leaves the help entirely.
  **The review found five defects, and they are one defect.** Each was a place where the
  change widened what a piece of code has to face and the assumption beside it did not
  move: the help's chord column was sized for the *built-in* chords while now rendering
  the *user's* (a rebind onto `Ctrl+Shift+PageDown` glued itself to its label); a config
  problem was discarded on the `--help` path, so a `--config` that does not exist printed
  the default key list as if it had been read; `-h` was deferred to the end of argument
  parsing and `-V` was not, so `-h -V` and `-V -h` disagreed; `Keymap::default` began
  depending on `toml`'s ordering being *irrelevant* with nothing checking that two rows
  never spell one chord - the same ordering `extend_from_pairs` explicitly refuses to
  rely on; and the README test read only the table's first column, leaving the chords
  named in the prose beside it (`Ctrl+G`, the whole clipboard paragraph) free to drift.
  The last is the one worth remembering: **a drift test that covers less than it appears
  to is worse than none**, because it is read as a guarantee. It scans the whole section
  now, and the clipboard prose was rewritten in terms of `mod` so it is checkable on both
  platforms at once rather than being a platform-split sentence no test could reach.
  *Verified:* the loop, plus a pty run against a config binding `g` and `ctrl+f` to `nop`
  and `mod+e` to the theme picker - `g` did not type itself (the consume rule, which is
  the whole difference between `nop` and an absent row), `X` beside it still typed, Ctrl+F
  opened nothing, Cmd+E opened the theme picker, and the saved file read exactly `Xhello`.
  `--help` under that config drops the find row and renders `Cmd+` on the clipboard rows.
  **Stage 2 - DONE.** Keymap **contexts**: a name per active scope - `editor`, the
  platform, and one per open overlay mirroring the compositor stack - looked up top-down,
  first match wins. The pickers, prompts, the confirmation and the query-replace walk stop
  matching key codes and start matching commands, so every key in the editor is bindable;
  the "a Ctrl chord falls through an overlay" heuristic copied into five layers is now a
  consequence of the table rather than a rule of its own; and two §11 debt entries are paid
  off with it. The design is a stack of names, not a predicate language, because this
  focus chain is the compositor's own ordered stack and is two deep - the reasoning, and
  the trigger that would overturn it, are in §10.5. It finishes the display rule with the
  two sites that need a surface context: the query-replace question and the four
  confirmations no longer spell `y`/`n`/`a`/`q` and `(y/N)` themselves. `keys-macos.toml`
  is deleted, absorbed into `keys.toml`'s own `[macos]` table - which is *read* on every
  platform, so the last `cfg!` in the keymap is gone and what varies is only which context
  is in the stack.
  Four details the design did not settle, decided while building it:
  - **The two unbound-key policies ask different questions, and the review caught the
    first attempt asking one.** A text surface asks "is this text?" and defers the rest;
    a *question* asks the narrower "is this somebody's shortcut?" and declines the rest,
    because a question is modal over the keyboard. Reading both as the printable test
    let Backspace at "discard your changes?" delete a character behind the question, and
    - because a confirmation is raised asynchronously onto whatever is already open -
    let a deferred Enter reach a picker buried under it and commit its highlighted row.
    §10.5 carries the rule and the two failures.
  - **A shifted letter is one chord however the terminal spells it.** Kitty reports `Y`
    *and* `SHIFT`; a classic terminal reports `Y` alone. `Chord::from_event` folds the
    letter to lower case and takes the shift from its *case*, so `"shift+y"` is one row
    that matches both - and is expressible at all, which it was not before (`Chord::parse`
    lowercases every token). It also makes `"ctrl+shift+f"` fire whether the terminal says
    `f` or `F`, which had been a latent hole. Text entry is untouched: it reads the key
    code, so an upper-case letter still types itself.
  - **The uppercase answer aliases are gone.** `Y`/`N`/`A` used to be hard-coded twins of
    `y`/`n`/`a`. As table rows they would be a second row for one answer, and the question
    now *renders its chords from those rows* - so an alias would decide what the question
    says (`Shift+N=no` won the tie-break). A shifted answer is simply not one of the named
    keys, and every key that is not named declines or ends the walk, which is the safe
    direction in both surfaces.
  - **A confirmation names only the chord that commits.** `(y/N)` became `(Y to confirm)`
    rather than `(Y/n)`: every other key declines, so naming the no key would suggest it
    is the only way to say no, and capitalizing the default stops working the moment the
    chord can be several characters long. The walk's answers became `Y=yes N=no A=all
    Q=quit` for the same reason - a rebound chord does not fit inside `(y)es`. M10
    restyles both; this stage only had to stop them being literals.
  *Verify:* a config that rebinds one key in each context, driven in a pty - the picker's
  highlight moving on the rebound key, `cancel` unbound in the picker and Esc still
  escaping through the editor context beneath it, a `nop`'d chord doing nothing at all
  (not typing itself), the query-replace question naming the rebound answers instead of
  `y`/`n`/`a`/`q`, and `--help` naming every chord the config actually resolved.

- **M10 - What the chrome says.** *(In progress - see §7.5 "What the chrome says" for
  the full design. **Done:** the status-bar audit and `indent_style`.)* M0-M8 built the surfaces; nothing had ever settled what each
  one is *allowed to put on screen*, and the answer had drifted: the head bar's right
  segment holds a line count, the status bar carries an internal document version, and the
  diagnostic count, the server's health and the attached grammar appear nowhere at all.
  One rule decides it - **a chrome cell is spent on what the user cannot otherwise learn,
  and presence is itself the signal** - together with three zones that stop the bars and
  the toasts duplicating each other: the head bar answers *what am I in*, the status bar
  answers *where am I and how is this written*, the toast and log answer *what just
  happened*.
  **Follows M9, and not by preference:** the dialog hint footer, the confirmation's
  answers and the empty state all render chords, and M9 is what makes rendering a chord
  something other than writing a literal (§10.5).
  Ordered so the cheap half lands first: the status-bar audit (**done**), the picker's
  count (**done**) / hint footer / match marks (**done**) / path rows, tab-name
  disambiguation, the empty state and the glyph profile are small; the head bar's state cluster, the message log, and the gutter
  diagnostics with their picker are medium. The one piece of **new scope** is an
  `indent_style` setting behind the `spaces:4` readout - a frontend change, since the
  frontend already decides what `insert_tab` inserts.
  **The status-bar audit, as built.** `insert_tab` stopped resolving to a literal `\t`
  in the keymap and became `InsertIndent`, filled in at *dispatch* from the live
  `indent_style` - the pattern `next_buffer` and `find_next` already use, and what keeps
  the setting off the binding. The caret diagnostic needed the one seam change in the
  milestone: `Decoration::Underline` carried a severity but not the server's `message`,
  so the squiggle could say only that *something* was wrong. It carries the message now
  (the additive-variant growth §5 designed for), and `DecorationSet::diagnostic_at` asks
  by the caret's **byte** rather than its line, so a line with two diagnostics reports
  the one the caret is inside. The rest rule turned out to need a *scheduled* repaint,
  not just a check: a caret that stops moving produces no event, so nothing would have
  asked for the frame that shows the message - the loop arms exactly one repaint when
  the deadline passes rather than painting every poll for 150ms.
  *Verify:* driven in a pty at **80 columns**, which is the budget the design is written
  against and the width the first draft failed - a nominal frame carrying tabs and a branch
  and nothing else, then the same file with the server indexing and three errors, with
  every added segment in the slot it was promised and the caret readout unmoved; a caret
  walked across a flagged line without the encoding readout strobing; `✗3` clicked to the
  diagnostics picker and back to the hit; a message read out of the log after its toast
  expired; and the whole run repeated under `glyphs = "ascii"` with no row misaligned.

Extensibility (§12.1) sits after the M0-M10 build order and is gated on that decision.

---

## 15. Workspace layout (target)

```
vortex/
  Cargo.toml            # workspace root (members = ["crates/*"])
  crates/
    core/               # NO crossterm/ratatui deps - compiler-enforced boundary
      src/
        buffer/         # crop wrapper, anchors, encoding/EOL, coordinate conversions
        selection/      # SelectionSet
        history/        # undo tree + coalescing
        syntax/         # tree-sitter, background reparse
        lsp/            # async-lsp client, UTF-16 position mapping
        action.rs       # Action enum (§12.2)
        view.rs         # ViewSnapshot / Notification
        decoration.rs   # Decoration / DecorationSet - one channel for all overlays (§5)
        editor.rs       # single-owner actor task
    tui/                # ratatui + crossterm; keymap keys -> Action; owns viewport + UI (§7.5)
      themes/           # the built-in theme files, compiled in with include_str! (§10.5)
      src/
        main.rs         # I/O shell: terminal setup, event loop, paint (kept thin)
        command.rs      # the dispatchable frontend command (§7.5)
        compositor.rs   # Layer trait + overlay stack + event routing (§7.5, job 2)
        picker.rs       # the shared fuzzy picker; palette.rs / filepicker.rs /
        palette.rs      #   themepicker.rs are thin item lists over it (§7.5)
        filepicker.rs
        themepicker.rs
        toast.rs        # the message surface (§7.5)
        components/     # (later) prompt, which-key, completion, hover (§7.5)
        chrome/         # (later) bufferline, indent guides, scrollbar (§7.5)
        layout.rs       # viewport + display-column math (testable)
        keymap.rs       # key -> Action lookup (data-driven, §10.5)
        config.rs       # the resolved Config value: Theme + Keymap (§10.5)
        theme.rs        # theme file format, discovery, loading (§10.5); the semantic
                        #   tag -> style map (§5) joins it with the decoration channel
        osc52.rs        # clipboard over the terminal (§11)
    proto/              # (later) serde + socket layer at the seam
  docs/
    SPEC.md             # this file
```

**Crate layout:** all crates live under `crates/` with **unprefixed directory names**
(`crates/core`, `crates/tui`, `crates/proto`), but each crate's **package name is
prefixed** in its `Cargo.toml` (`vortex-core`, `vortex-tui`, `vortex-proto`). The
workspace root declares `members = ["crates/*"]`. Internal deps reference the package
name with a path, e.g. in `crates/tui/Cargo.toml`:

```toml
[dependencies]
vortex-core = { path = "../core" }
```

This split gives clean directory names *and* unambiguous, publishable package names -
`vortex-core` never collides with Rust's built-in `core`, while the tree stays readable.

The `core` crate having **zero terminal dependencies** is the compile-time guarantee that
view logic cannot leak in - stronger than discipline.
