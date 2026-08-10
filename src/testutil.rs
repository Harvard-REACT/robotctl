//! A minimal self-cleaning temporary directory for tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, fs};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// `tag` only makes a leaked directory identifiable if a test aborts before `Drop` runs;
    /// uniqueness comes from the pid and counter.
    pub fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "robotctl-test-{tag}-{}-{unique}",
            std::process::id()
        ));

        fs::create_dir_all(&path).expect("could not create test temp dir");

        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a failure here must not mask the assertion failure that caused it.
        let _ = fs::remove_dir_all(&self.path);
    }
}
