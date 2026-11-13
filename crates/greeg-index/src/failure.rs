//! The last failed build (`BUILD_FAILED`, beside the build marker): queries
//! back off instead of starting the same build again on every call, and
//! `doctor` shows why. A build that succeeds removes it.

use crate::{create_private, format, now_ms, persist};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

const NAME: &str = "BUILD_FAILED";
/// The wait after the first failure, doubled after each one up to [`MAX_MS`].
const FIRST_MS: u64 = 30_000;
const MAX_MS: u64 = 3_600_000;
/// The error text kept, in bytes.
const MAX_ERROR: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildFailure {
    pub error: String,
    pub at_unix_ms: u64,
    /// Failed builds in a row.
    pub attempts: u32,
    /// A query has said so on stderr.
    #[serde(default)]
    pub reported: bool,
}

impl BuildFailure {
    /// When a background build may start again (unix ms).
    pub fn retry_at(&self) -> u64 {
        let wait = FIRST_MS
            .saturating_mul(1 << self.attempts.saturating_sub(1).min(16))
            .min(MAX_MS);
        self.at_unix_ms.saturating_add(wait)
    }

    /// Is `now` inside the back-off window? Not when the record is from the
    /// future, left by a clock that stepped back.
    pub fn backing_off(&self, now: u64) -> bool {
        self.at_unix_ms <= now && now < self.retry_at()
    }
}

/// The record in `dir`, if a build there failed last.
pub fn read(dir: &Path) -> Option<BuildFailure> {
    serde_json::from_slice(&std::fs::read(dir.join(NAME)).ok()?).ok()
}

/// Record that a build in `dir` failed with `error`, one attempt more than
/// the record before. Nothing is written under `--no-persist`.
pub fn record(dir: &Path, error: &str) -> anyhow::Result<()> {
    let attempts = read(dir).map_or(0, |f| f.attempts) + 1;
    let mut error = error.replace(['\n', '\r'], " ");
    if error.len() > MAX_ERROR {
        let mut end = MAX_ERROR;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
        error.push('…');
    }
    write(
        dir,
        &BuildFailure {
            error,
            at_unix_ms: now_ms(),
            attempts,
            reported: false,
        },
    )
}

/// Note that a query has told the user about `f`.
pub fn mark_reported(dir: &Path, f: &BuildFailure) {
    let _ = write(
        dir,
        &BuildFailure {
            reported: true,
            ..f.clone()
        },
    );
}

/// Remove the record: a build in `dir` succeeded.
pub fn clear(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(NAME));
}

fn write(dir: &Path, f: &BuildFailure) -> anyhow::Result<()> {
    if !persist::allowed() {
        return Ok(());
    }
    let path = dir.join(NAME);
    let tmp = format::tmp_path(&path);
    create_private(&tmp)?.write_all(&serde_json::to_vec(f)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_up_to_an_hour_and_a_clock_step_back_ends_it() {
        let f = |attempts| BuildFailure {
            at_unix_ms: 1_000_000,
            attempts,
            ..Default::default()
        };
        assert_eq!(f(1).retry_at(), 1_030_000);
        assert_eq!(f(2).retry_at(), 1_060_000);
        assert_eq!(f(3).retry_at(), 1_120_000);
        assert_eq!(f(40).retry_at(), 1_000_000 + MAX_MS);
        assert!(f(1).backing_off(1_029_999));
        assert!(!f(1).backing_off(1_030_000));
        assert!(!f(1).backing_off(999_999), "from the future");
    }

    #[test]
    fn a_record_counts_failures_in_a_row_and_a_success_clears_it() {
        let d = std::env::temp_dir().join(format!("greeg-failure-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        clear(&d);
        record(&d, "first\nline").unwrap();
        record(&d, &"x".repeat(2000)).unwrap();
        let f = read(&d).unwrap();
        assert_eq!(f.attempts, 2);
        assert!(f.error.len() <= MAX_ERROR + '…'.len_utf8());
        assert!(!f.reported);
        mark_reported(&d, &f);
        assert!(read(&d).unwrap().reported);
        clear(&d);
        assert!(read(&d).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
