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
Frontend  ◀─ ViewSnapshot ────     undo tree, syntax trees, styles
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

- One task owns `SelectionSet`, buffers, undo tree, syntax trees, styles. Edits mutate
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
| Config | `toml` + `serde` | Helix-style |
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
  - *Not yet run end-to-end:* smol + async-lsp + a real language server is doc-supported
    but unbuilt here. **Validated by milestone M2 (§14).**
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
    styles: Arc<StyleMap>,        // Arc-shared - see representation below
    // ... line-count, dirty hint (changed line range) for partial repaint
}
```

- **Every field is cheaply shared, not just `text`.** `selections` and `styles` are behind
  `Arc` too, so building a snapshot is a handful of atomic ref-count bumps regardless of
  file size or match count - *not* an O(spans) or O(selections) deep clone per frame. (The
  earlier draft only shared `text`; sharing just the rope while deep-cloning a
  `Vec<span>`/`Vec<selection>` would silently reintroduce per-frame cost - the exact thing
  this model exists to avoid.)
- **Style representation (resolve the A↔C contradiction):** `StyleMap` stores spans as
  **anchors internally** (so they survive edits without a reparse), and the frontend
  **resolves anchors to concrete ranges lazily, for the visible line range only** - never
  eagerly for the whole file. This bounds resolution cost to viewport size and keeps
  snapshot construction O(1)-ish.
- **Frontend owns the viewport.** It holds the latest `ViewSnapshot` and, on its own render
  tick, reads exactly the visible line range from `text` + `styles` and paints. Scrolling
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

### Styling pipeline (why styles may lag text by a frame, and why that's fine)

Tree-sitter highlighting and LSP diagnostics are **too expensive to recompute
synchronously per keystroke**. Flow:

1. Edit applies to the buffer immediately (single owner, synchronous). Core emits a
   snapshot with **text updated now**, styles carried forward / best-effort remapped
   through anchors.
2. A background task reparses on the cheap snapshot clone; when done, the core emits a new
   snapshot with **refreshed styles** at a later version.
3. Result: **text is never stale** (user always sees what they typed instantly);
   highlighting may trail by a frame or two. This is the correct trade - the reverse
   (blocking on highlight before showing text) is exactly Xi's latency mistake.

### Frame budget

Frontend coalesces rapid input: it may receive many snapshots/inputs but paints at most
once per frame budget (target ~8-16ms). This is *when* it calls the loop it already owns
(§7), not a custom renderer.

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
stays local. Earn the compositor by outgrowing ratatui.

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
concerns (§2.2, §5) and never cross the seam. Two surfaces are configurable from the
start of the design, even though file loading itself lands at **M5**:

- **Styles (theme).** Colors/attributes for the non-text chrome - head bar, status bar,
  and the line-number gutter (active vs inactive line). A future syntax theme maps
  tree-sitter capture names to styles (§5 `StyleMap`), but that is a separate table from
  the chrome theme here.
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
  - **Text entry is a fallback, not a binding:** an unbound printable char with no Ctrl
    inserts itself, so the map never enumerates every letter.
  - **Open:** modal-vs-modeless, chord *sequences* (multi-key), and per-mode maps are the
    remaining design, drafted alongside the §12.2 `Action` vocabulary.

**Seam, not yet a loader (current state).** The config lives behind a single resolved
`Config` value built once at frontend startup (next to argv, before the first frame) and
threaded into the render/input paths - **not** scattered constants. Today it is the
built-in `Default`; M5 replaces that construction with `Config::load(path)` deserializing
the user's file and falling back to the defaults for any unset field. Because every call
site already reads from the `Config` value, adding file loading touches only that one
construction point. A `--config <path>` flag rides the same argv parser. This mirrors the
Tier-3 / CRDT move: build the swap-ready seam now, defer the feature.

---

## 11. Deferred (not silently skipped)

- **Full CRDT / replica model** - only when remote or collaboration is real. Anchor API is
  kept swap-ready (§2.1); not built now.
- **Out-of-process RPC** - the channel is the seam; add the wire when a non-Rust or remote
  frontend exists (§1). The transport ships the `Delta` stream the core already produces
  (§5) - no snapshot reconstruction needed.
- **Custom compositor** - earn it by outgrowing ratatui (§7).
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
- **Search + regex.** `select-all-matches` / `split-on-regex` (§12.2) imply a `regex`
  dependency and a search subsystem in `vortex-core`. Add the `regex` crate to the stack
  when this lands; incremental/streaming search over the rope. Target: M3 band.
- **Keymap configuration.** The data-driven keymap and its chord/command string format
  exist now (§10.5); what remains is loading a user file into it (M5, rides the same
  `toml` seam) and the richer *modal* design - chord sequences, per-mode maps, modal vs
  modeless - drafted alongside §12.2's `Action` vocabulary. Target: M1+.

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

---

## 13. Test strategy

The headless, message-driven core (§1) makes this concrete:

- **Golden/interaction tests:** feed an `Action` script, assert on the emitted
  `ViewSnapshot`/`Notification` sequence. No terminal, no PTY, no snapshot-image
  fragility. This is the primary suite and covers the entire editing model. **Assert on
  projections, not whole snapshots** - check text + resolved selection positions +
  notifications, but not the raw `styles` map, which shifts with tree-sitter grammar
  versions and would make tests brittle. Style *correctness* is covered separately with
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
  standard). *Verify exact flags at setup - `--fail-under-lines` is confirmed; branch
  coverage (`--branch`) may require nightly, confirm before relying on it.*
- **Ratchet, not a fixed number.** The gate is "coverage must not decrease" plus a floor.
  This encodes "max at each turn" without forcing tests on trivial glue. New code lands
  with its tests in the same change, so the ratchet only ever climbs.
- **Asymmetric floors, because the architecture is asymmetric:**
  - **`vortex-core` ≥ 90% lines** (target higher). The core is headless and message-driven
    (§1) specifically so it is almost fully testable via `Action`→`ViewSnapshot` scripts -
    there is no excuse for low core coverage.
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
- **M2 - Async runtime + LSP smoke.** smol executor; `async-lsp` spawns a real server
  (e.g. `rust-analyzer`), completes `initialize`, receives one diagnostic, maps its UTF-16
  position correctly. **Validates the one unproven stack assumption (§3).** *Verify:* a
  diagnostic underlines the right span.
- **M3 - Anchors + undo tree + multi-cursor.** Full `SelectionSet`, anchor layer,
  coalesced undo tree. *Verify:* property tests (§13) pass; multi-cursor edit + undo works
  in-terminal.
- **M4 - Syntax highlighting.** tree-sitter background reparse on snapshots feeding
  `styles`. *Verify:* highlights appear, text never lags input.
- **M5 - File handling hardening.** encoding/EOL preservation, external-change conflicts,
  save-failure handling (§8, §10). *Verify:* fixture round-trips + fault-injection.

Extensibility (§12.1) is post-M5 and gated on that decision.

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
        editor.rs       # single-owner actor task
    tui/                # ratatui + crossterm; keymap keys -> Action; owns viewport
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
