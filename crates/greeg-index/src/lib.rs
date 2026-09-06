//! Persistent per-repository index (DESIGN.md §2–§5): file table, file-level
//! trigram postings, delta segments, tombstones, freshness.
//!
//! Phase 1 (grams) and phase 2 (symbols, spans, graph). Everything on disk is little-endian,
//! fixed-layout, and read through `mmap` without deserialization.

pub mod build;
pub mod format;
pub mod fresh;
pub mod gram;
pub mod index;
pub mod lock;
pub mod plan;
pub mod resolve;
pub mod symtab;

pub use index::Index;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u16 = 4;

/// Manifest: JSON, small, rewritten atomically on every publish/check.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u16,
    pub root: String,
    pub generation: u32,
    pub phase1: bool,
    /// Symbols, spans and graph published (DESIGN.md §4.1 phase 2).
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
    /// FSEvents event id at publish or last successful check (macOS).
    pub fsevents_id: u64,
    /// Last time the index was verified fresh (unix ms), for the TTL.
    pub verified_unix_ms: u64,
    /// Number of delta segments currently applied.
    pub deltas: u32,
    pub tombstones: u32,
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

/// Where the index for `root` lives (DESIGN.md §2.2).
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
