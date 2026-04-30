//! Shared test utilities for rusty-claude-cli integration tests.
//!
//! Removes duplication of `unique_temp_dir`, `assert_success`, and
//! `TEMP_COUNTER` across the five CLI test files.

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a unique temporary directory for a test, avoiding collisions.
pub fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ninmu-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}

/// Assert that a CLI process exited successfully, printing stdout/stderr on
/// failure.
pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
