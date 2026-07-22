//! Shared helpers for this crate's unit tests (compiled only under `cfg(test)`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory removed on drop, so a test that touches the real filesystem
/// cleans up even if an assertion panics first (a bare trailing `remove_dir_all`
/// would leak the dir on failure). Name mixes pid + a counter to avoid
/// collisions across parallel tests.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("vortex-tui-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// Create `rel` (and any missing parent directories) under the temp root.
    pub fn file(&self, rel: &str, body: &str) {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The concatenated symbols of row `y`, for substring assertions on a painted row.
pub fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area().width)
        .map(|x| buf.cell((x, y)).unwrap().symbol())
        .collect()
}
