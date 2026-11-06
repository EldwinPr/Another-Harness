//! Test-only helpers. Tests never touch the repository or the home directory:
//! every test that needs files works in its own scratch directory under the
//! system temp folder, removed when the test ends (context/coder-only/testing.md).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// Rust note: a `static` lives for the whole program. An atomic counter can be
// bumped safely from tests running in parallel threads.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory that is deleted again when this value goes out of scope.
pub(crate) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(crate) fn new(tag: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "another-harness-test-{}-{tag}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        // Canonical form, so comparisons with what `pwd` prints are reliable
        // even when the temp folder is reached through a symlink.
        let path = path.canonicalize().expect("canonicalize scratch dir");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

// Rust note: `Drop` runs automatically when a value is destroyed, even if the
// test failed half-way. That is how the folder always gets cleaned up.
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
