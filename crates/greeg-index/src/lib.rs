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
pub mod private;
pub mod rel;
pub mod resolve;
pub mod skipped;
pub mod snapshot;
pub mod symtab;
pub mod words;

pub use index::Index;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The on-disk layout. Every change to what a build or refresh writes gets a
/// new number, released or not, so binaries of different layouts never share
/// files (`format_dir`); `layout_fingerprint_matches_format_version` enforces it.
pub const FORMAT_VERSION: u32 = 9;

/// Create an index directory and any missing parents owner-only (0700).
/// Existing directories are left as they are, never chmodded.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Options that create files owner-only (0600); the caller adds the mode of
/// access and then calls [`owner_only`] on a file it created.
pub fn private_file() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.mode(0o600);
    o
}

/// Set a file greeg just created to exactly 0600: the umask can remove bits
/// from the creation mode.
pub fn owner_only(f: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
}

/// Create or truncate a file greeg owns in an index directory, owner-only.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let f = private_file()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    owner_only(&f)?;
    Ok(f)
}

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
    pub format: u32,
    /// The root as text, for display.
    pub root: String,
    /// The root this index was built for; an index is used only for it.
    #[serde(default)]
    pub root_id: RootId,
    /// Builds published in this directory so far.
    pub generation: u32,
    /// This build's random identity; its files live in `g-<epoch>/`
    /// (`snapshot.rs`), and every component header carries it.
    pub epoch: u64,
    /// Publication within the build: 1 after phase 1, 2 after phase 2.
    pub seq: u32,
    /// The publication sequence of each base component.
    pub components: snapshot::Components,
    /// Superseded entries, removed once their grace period is over.
    #[serde(default)]
    pub retired: Vec<snapshot::Retired>,
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
    /// The `skipped` record for this index, relative to its directory. Empty
    /// in manifests written before it was recorded: coverage is then unknown.
    #[serde(default)]
    pub skipped: String,
    /// `STAMP_NO_INO` once a check found that this file system does not keep
    /// inode numbers; empty otherwise (`fresh::classify_all`).
    #[serde(default)]
    pub stamp_mode: String,
}

/// `Manifest::stamp_mode` when freshness no longer compares inode numbers.
pub const STAMP_NO_INO: &str = "no-ino";

/// A repository root's identity: its canonical path's bytes (hashed), device
/// and inode. A root replaced at the same path (a new clone) differs too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootId {
    pub hash: String,
    pub dev: u64,
    pub ino: u64,
}

impl RootId {
    pub fn of(root: &Path) -> Option<RootId> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let real = std::fs::canonicalize(root).ok()?;
        let md = std::fs::metadata(&real).ok()?;
        Some(RootId {
            hash: blake3::hash(real.as_os_str().as_bytes()).to_hex()[..32].to_string(),
            dev: md.dev(),
            ino: md.ino(),
        })
    }
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

/// The directory that holds everything greeg keeps for `root`: sessions, and
/// one index per layout in `v<N>/` (ARCHITECTURE.md). `GREEG_INDEX_DIR`
/// overrides it, as `--index-dir` does (`repo_dir`).
pub fn repo_dir_for(root: &Path) -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("GREEG_INDEX_DIR") {
        return Ok(PathBuf::from(d));
    }
    let real =
        std::fs::canonicalize(root).with_context(|| format!("canonicalize {}", root.display()))?;
    Ok(cache_base()?.join(repo_dir_name(&real)))
}

/// `<name>-<16 hex>` for a canonical root. The hash covers the path's bytes,
/// so roots that differ only in bytes that are not UTF-8 stay apart; for a
/// UTF-8 path it is the name releases before 0.8 used.
fn repo_dir_name(real: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let hex = blake3::hash(real.as_os_str().as_bytes()).to_hex();
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    format!("{}-{}", sanitize(&name), &hex[..16])
}

/// This layout's index inside a repository directory. Other layouts' files,
/// including the top-level files of releases before 0.8, are never touched.
pub fn format_dir(repo: &Path) -> PathBuf {
    repo.join(format!("v{FORMAT_VERSION}"))
}

/// The repository directory an index directory from `format_dir` belongs to.
pub fn repo_of(index_dir: &Path) -> &Path {
    index_dir.parent().unwrap_or(index_dir)
}

/// `repo_dir_for`, or the directory `--index-dir` named.
pub fn repo_dir(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(d) => Ok(d.to_path_buf()),
        None => repo_dir_for(root),
    }
}

/// Where this layout's index for `root` lives.
pub fn index_dir(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    Ok(format_dir(&repo_dir(root, explicit)?))
}

pub fn index_dir_for(root: &Path) -> Result<PathBuf> {
    index_dir(root, None)
}

/// Mark an index directory as greeg's own (`OWNER`, 0600), so cleanup and
/// purge can tell it from anything else. Kept once written.
pub fn write_owner(dir: &Path) -> Result<()> {
    let path = dir.join("OWNER");
    if path.exists() {
        return Ok(());
    }
    let tmp = format::tmp_path(&path);
    create_private(&tmp)?
        .write_all(format!("greeg {} {FORMAT_VERSION}\n", env!("CARGO_PKG_VERSION")).as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
    create_private(&tmp)?.write_all(&serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp, dir.join("manifest"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn utf8_roots_keep_the_directory_names_of_earlier_releases() {
        let real = Path::new("/home/dev/my repo");
        let lossy = blake3::hash(real.to_string_lossy().as_bytes()).to_hex();
        assert_eq!(repo_dir_name(real), format!("my_repo-{}", &lossy[..16]));
    }

    #[test]
    fn non_utf8_roots_get_distinct_index_directories() {
        use std::os::unix::ffi::OsStrExt;
        let a = Path::new(std::ffi::OsStr::from_bytes(b"/src/r\xff"));
        let b = Path::new(std::ffi::OsStr::from_bytes(b"/src/r\xfe"));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert_ne!(repo_dir_name(a), repo_dir_name(b));
    }

    #[test]
    fn open_regular_refuses_symlinks_and_fifos_without_blocking() {
        let d = (0..1000)
            .map(|n| {
                std::env::temp_dir().join(format!("greeg-open-regular-{}-{n}", std::process::id()))
            })
            .find(|d| fs::create_dir(d).is_ok())
            .unwrap();
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
