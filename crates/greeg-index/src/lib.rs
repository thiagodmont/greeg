//! Persistent per-repository index (ARCHITECTURE.md): file table, file-level
//! trigram and word postings, delta segments, tombstones, freshness.
//!
//! Phase 1 (grams) and phase 2 (symbols, spans, graph). Everything on disk is little-endian,
//! fixed-layout, and read through `mmap` without deserialization.

pub mod build;
pub mod external;
pub mod format;
pub mod fresh;
pub mod gram;
pub mod ignores;
pub mod index;
pub mod lock;
pub mod plan;
pub mod resolve;
pub mod symtab;
pub mod words;

pub use index::Index;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u16 = 5;

/// Open an indexed path only while it is still a regular file: a symlink is
/// not followed and a FIFO or device cannot block the open. This checks the
/// file itself, not its parent directories.
pub fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(f)
}

/// Manifest: JSON, small, rewritten atomically on every publish/check.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u16,
    pub root: String,
    pub generation: u32,
    pub phase1: bool,
    /// Symbols, spans and graph published (phase 2, ARCHITECTURE.md).
    #[serde(default)]
    pub phase2: bool,
    #[serde(default)]
    pub symbols: u32,
    #[serde(default)]
    pub edges: u32,
    #[serde(default)]
    pub parse_fallbacks: u32,
    #[serde(default)]
    pub phase2_ms: f64,
    pub files: u32,
    pub dirs: u32,
    pub source_bytes: u64,
    pub built_unix_ms: u64,
    pub build_ms: f64,
    /// Peak resident set size of the build process, in bytes (0 = unknown).
    /// Recorded so the benchmark can gate on build memory, not only on time.
    #[serde(default)]
    pub peak_rss: u64,
    /// The phase-1 postings reached the byte budget and were merged through
    /// spill segments rather than in memory (`external.rs`).
    #[serde(default)]
    pub spilled: bool,
    /// FSEvents event id at publish or last successful check (macOS).
    pub fsevents_id: u64,
    /// Last time the index was verified fresh (unix ms), for the TTL.
    pub verified_unix_ms: u64,
    /// Number of delta segments currently applied.
    pub deltas: u32,
    pub tombstones: u32,
    /// `ignores::digest` when the walk ran. Empty in manifests written before
    /// it was recorded: those are not checked until their next full build.
    #[serde(default)]
    pub ignore_inputs: String,
}

/// The user cache directory greeg owns: `~/Library/Caches/greeg` on macOS,
/// `$XDG_CACHE_HOME/greeg` or `~/.cache/greeg` elsewhere. Per-repo index
/// directories and the opt-in stats live under it.
pub fn cache_base() -> Result<PathBuf> {
    Ok(if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME").context("HOME")?).join("Library/Caches/greeg")
    } else if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(x).join("greeg")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache/greeg")
    })
}

/// Where the index for `root` lives (ARCHITECTURE.md).
pub fn index_dir_for(root: &Path) -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("GREEG_INDEX_DIR") {
        return Ok(PathBuf::from(d));
    }
    let real =
        std::fs::canonicalize(root).with_context(|| format!("canonicalize {}", root.display()))?;
    let hash = blake3::hash(real.to_string_lossy().as_bytes());
    let hex = hash.to_hex();
    let base = cache_base()?;
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    Ok(base.join(format!("{}-{}", sanitize(&name), &hex[..16])))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

/// Peak resident set size of this process, in bytes, or `None` if the kernel
/// will not say. `getrusage(RUSAGE_SELF).ru_maxrss` is a high-water mark that
/// never falls, so it is the build's peak and not its current use; the units
/// differ by platform (bytes on macOS, kilobytes on Linux).
///
/// Above 1 MiB the build reads through `mmap`, and those pages are
/// file-backed and reclaimable, so on a tree of large files this overstates
/// what the process actually holds. Every corpus greeg is measured on is
/// small files, where the two agree.
pub fn peak_rss_bytes() -> Option<u64> {
    // SAFETY: `getrusage` only writes the `rusage` it is given.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return None;
    }
    let raw = ru.ru_maxrss as u64;
    Some(if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    })
}

/// `123.4 MB`-style rendering of a byte count, binary units as everywhere
/// else greeg prints a size (`greeg doctor`'s `fmt_size`), so the build line
/// and the doctor line agree on the same number.
pub fn fmt_bytes(n: u64) -> String {
    let mb = n as f64 / 1048576.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.1} MB")
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn read_manifest(dir: &Path) -> Option<Manifest> {
    let s = std::fs::read(dir.join("manifest")).ok()?;
    let m: Manifest = serde_json::from_slice(&s).ok()?;
    if m.format != FORMAT_VERSION {
        return None;
    }
    Some(m)
}

/// Rewrite the manifest atomically. Callers hold the writer lock (`lock::writer`).
pub fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    let tmp = format::tmp_path(&dir.join("manifest"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp, dir.join("manifest"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn open_regular_refuses_symlinks_and_fifos_without_blocking() {
        let d = std::env::temp_dir().join(format!("greeg-open-regular-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(d.join("file"), d.join("link")).unwrap();
        let fifo = std::ffi::CString::new(d.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(open_regular(&d.join("file")).is_ok());
        assert!(open_regular(&d.join("link")).is_err());
        assert!(open_regular(&d.join("fifo")).is_err());
        assert!(open_regular(&d).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
