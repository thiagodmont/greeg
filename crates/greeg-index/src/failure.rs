//! The last failed build (`BUILD_FAILED`, beside the build marker): queries
//! back off instead of starting the same build again on every call, and
//! `doctor` shows why. A build writes it when it starts, so one that is
//! killed counts too, and removes it when it succeeds. While the build
//! marker is held, the record describes the build still running.

use crate::{create_private, format, now_ms, persist};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

const NAME: &str = "BUILD_FAILED";
/// The record a query has told the user about, by [`BuildFailure::key`].
const REPORTED: &str = "BUILD_FAILED.reported";
/// What a build that has not finished says.
pub const UNFINISHED: &str = "the build did not finish";
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
    /// The root the builds were for: an `--index-dir` can serve several.
    pub root: String,
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

    fn key(&self) -> String {
        format!("{} {}", self.at_unix_ms, self.attempts)
    }
}

/// How a root is named in the record.
fn root_name(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The record in `dir` for builds of `root`, if one failed last.
pub fn read(dir: &Path, root: &Path) -> Option<BuildFailure> {
    let f: BuildFailure = serde_json::from_slice(&std::fs::read(dir.join(NAME)).ok()?).ok()?;
    (f.root == root_name(root)).then_some(f)
}

/// A build of `root` in `dir` starts: it counts as failed until it says
/// otherwise. An error means `dir` cannot be written, so the build cannot
/// publish either. Nothing is written under `--no-persist`.
pub fn begin(dir: &Path, root: &Path) -> anyhow::Result<()> {
    let attempts = read(dir, root).map_or(0, |f| f.attempts).saturating_add(1);
    write(dir, root, UNFINISHED, attempts)
}

/// The build `begin` recorded failed with `error`.
pub fn fail(dir: &Path, root: &Path, error: &str) -> anyhow::Result<()> {
    let attempts = read(dir, root).map_or(1, |f| f.attempts);
    write(dir, root, error, attempts)
}

/// Remove the record: a build in `dir` succeeded.
pub fn clear(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(NAME));
    let _ = std::fs::remove_file(dir.join(REPORTED));
}

/// Has a query told the user about `f`?
pub fn reported(dir: &Path, f: &BuildFailure) -> bool {
    std::fs::read_to_string(dir.join(REPORTED)).is_ok_and(|k| k == f.key())
}

/// Note that a query has told the user about `f`. A separate file, so this
/// never writes over a record a build has replaced or removed since.
pub fn mark_reported(dir: &Path, f: &BuildFailure) {
    let _ = write_file(dir, REPORTED, f.key().as_bytes());
}

fn write(dir: &Path, root: &Path, error: &str, attempts: u32) -> anyhow::Result<()> {
    let mut error = error.replace(['\n', '\r'], " ");
    if error.len() > MAX_ERROR {
        let mut end = MAX_ERROR;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
        error.push('…');
    }
    let f = BuildFailure {
        error,
        at_unix_ms: now_ms(),
        attempts,
        root: root_name(root),
    };
    write_file(dir, NAME, &serde_json::to_vec(&f)?)
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    if !persist::allowed() {
        return Ok(());
    }
    let path = dir.join(name);
    let tmp = format::tmp_path(&path);
    create_private(&tmp)?.write_all(bytes)?;
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
        assert_eq!(f(u32::MAX).retry_at(), 1_000_000 + MAX_MS);
        assert!(f(1).backing_off(1_029_999));
        assert!(!f(1).backing_off(1_030_000));
        assert!(!f(1).backing_off(999_999), "from the future");
    }

    #[test]
    fn a_record_counts_failures_in_a_row_for_its_root_and_a_success_clears_it() {
        let d = std::env::temp_dir().join(format!("greeg-failure-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (root, other) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        clear(&d);
        begin(&d, &root).unwrap();
        assert_eq!(read(&d, &root).unwrap().error, UNFINISHED);
        fail(&d, &root, "first\nline").unwrap();
        begin(&d, &root).unwrap();
        fail(&d, &root, &"x".repeat(2000)).unwrap();
        let f = read(&d, &root).unwrap();
        assert_eq!(f.attempts, 2);
        assert!(f.error.len() <= MAX_ERROR + '…'.len_utf8());
        assert!(read(&d, &other).is_none(), "another root's builds");
        assert!(!reported(&d, &f));
        mark_reported(&d, &f);
        assert!(reported(&d, &f));
        begin(&d, &root).unwrap();
        assert!(!reported(&d, &read(&d, &root).unwrap()), "a newer record");
        clear(&d);
        assert!(read(&d, &root).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
