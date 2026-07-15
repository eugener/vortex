//! Core -> frontend messages: `ViewSnapshot` (render state) and `Notification`
//! (discrete events). See SPEC §5 (render model) and §6 (channels).
//!
//! M0 carries only what proves the seam. From M1 the core's primary "what
//! changed" output is a `Delta` stream (SPEC §5) - lossless, ordered, and the
//! wire protocol for remote frontends; `ViewSnapshot` becomes a *derived*,
//! `Arc`-shared convenience for local frontends (`text: crop::Rope`, `selections:
//! Arc<[Selection]>`, `styles: Arc<StyleMap>`), cheap to build regardless of file
//! size. `Delta` is not introduced until M1 has an edit to produce one.

/// Identifies a buffer. Versions are per-buffer (SPEC §5), so an edit in one
/// buffer never invalidates another's anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u64);

/// Immutable render state a *local* frontend paints from - a derived convenience,
/// not the authoritative change log (that is the `Delta` stream, SPEC §5).
/// Latest-wins: the frontend only ever needs the newest (SPEC §5, §6). M0 fields
/// are placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ViewSnapshot {
    pub buffer_id: BufferId,
    /// Per-buffer monotonic counter; the frontend ignores snapshots older than
    /// the newest it holds.
    pub version: u64,
    /// Placeholder for M0. Becomes the `crop::Rope` + selections + styles in M1.
    pub text: String,
}

/// Discrete core -> frontend events (errors, status, prompts). Self-contained
/// on purpose: a notification may arrive out of order with snapshots, so it
/// carries the `buffer_id`/`version` it refers to rather than assuming a paired
/// snapshot is present (SPEC §6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notification {
    /// The core has stopped its loop and will send nothing further.
    ShuttingDown,
}
