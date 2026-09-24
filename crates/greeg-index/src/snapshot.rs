//! Snapshots (ARCHITECTURE.md): every build writes its components into its
//! own directory, `g-<epoch>/`, named by kind and publication sequence and
//! never renamed over, so the manifest a reader read names one whole
//! snapshot. Superseded entries are retired, and deleted once a reader that
//! read an older manifest has had time to open what it names; entries no
//! manifest names are orphans of interrupted writers.

use crate::Manifest;
use crate::format::{
    COMP_FILES, COMP_GRAMS, COMP_GRAPH, COMP_SKIPPED, COMP_SPANS, COMP_SYMBOLS, COMP_WORDS,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Publication sequence of what phase 1 writes, and of what phase 2 writes.
pub const SEQ_PHASE1: u32 = 1;
pub const SEQ_PHASE2: u32 = 2;
/// How long a retired entry stays: a reader that read the manifest naming
/// it opens it well within this.
pub const RETIRE_MS: u64 = 30_000;
/// Unnamed entries younger than this may still belong to a writer at work.
pub const ORPHAN_MS: u64 = 10 * 60_000;
/// Removals per cleanup pass, so no publication waits on a large backlog.
pub const MAX_REMOVALS: usize = 64;
/// `Index::open` attempts while the snapshot keeps changing under it.
pub const OPEN_ATTEMPTS: usize = 3;
/// Retired builds kept whatever their age, so a burst of rebuilds does not
/// hold a copy of the index per rebuild; a reader that loses one retries.
pub const RETIRED_BUILDS: usize = 2;

/// A published snapshot: the build, its publication, and the deltas on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub epoch: u64,
    pub seq: u32,
    pub deltas: u32,
}

/// The publication sequence of each base component; 0 when absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Components {
    pub files: u32,
    pub grams: u32,
    pub words: u32,
    pub symbols: u32,
    pub spans: u32,
    pub graph: u32,
}

/// A superseded entry (a file, or a whole build directory), relative to the
/// layout directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub name: String,
    pub since_ms: u64,
}

/// A build's directory name.
pub fn gen_dir(epoch: u64) -> String {
    format!("g-{epoch:016x}")
}

fn kind_name(comp: u8) -> &'static str {
    match comp {
        COMP_FILES => "files",
        COMP_GRAMS => "grams",
        COMP_WORDS => "words",
        COMP_SYMBOLS => "symbols",
        COMP_SPANS => "spans",
        COMP_GRAPH => "graph",
        COMP_SKIPPED => "skipped",
        _ => "unknown",
    }
}

impl Manifest {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            epoch: self.epoch,
            seq: self.seq,
            deltas: self.deltas,
        }
    }

    /// A base component's name (relative to the layout directory) and
    /// sequence, when it is published.
    pub fn component(&self, comp: u8) -> Option<(String, u32)> {
        let c = &self.components;
        let seq = match comp {
            COMP_FILES => c.files,
            COMP_GRAMS => c.grams,
            COMP_WORDS => c.words,
            COMP_SYMBOLS => c.symbols,
            COMP_SPANS => c.spans,
            COMP_GRAPH => c.graph,
            _ => 0,
        };
        (seq != 0).then(|| (self.named(comp, seq), seq))
    }

    /// The name of a `comp` file of this build published at `seq`.
    pub fn named(&self, comp: u8, seq: u32) -> String {
        format!("{}/{}.{seq}.bin", gen_dir(self.epoch), kind_name(comp))
    }

    /// Delta segment `n` (1-based) of this build.
    pub fn delta(&self, n: u32) -> String {
        format!("{}/d-{n:04}.bin", gen_dir(self.epoch))
    }

    /// The skipped record a delta segment `n` wrote.
    pub fn delta_skipped(&self, n: u32) -> String {
        format!("{}/d-{n:04}.skipped", gen_dir(self.epoch))
    }
}

/// A new build's epoch: random, nonzero and unlike the previous build's.
/// `GREEG_DEBUG_FIXED_STAMPS` derives it from the generation instead, so the
/// layout fingerprint is the same on every run.
pub fn new_epoch(previous: Option<&Manifest>, generation: u32) -> u64 {
    let e = if crate::build::fixed_identity() {
        u64::from(generation).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    } else {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut h = blake3::Hasher::new();
        h.update(&std::process::id().to_le_bytes());
        h.update(&crate::build::now_ns().to_le_bytes());
        h.update(
            &N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_le_bytes(),
        );
        let here = 0u8;
        h.update(&(&here as *const u8 as usize).to_le_bytes());
        u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
    };
    match e {
        0 => 1,
        e if previous.is_some_and(|m| m.epoch == e) => e ^ 1,
        e => e,
    }
}

/// The `retired` list of a manifest about to be published, and the entries
/// whose grace period is over: removed once that manifest is published.
pub fn retire(
    previous: &[Retired],
    new: impl IntoIterator<Item = String>,
    now_ms: u64,
) -> (Vec<Retired>, Vec<String>) {
    let (due, mut keep): (Vec<Retired>, Vec<Retired>) = previous
        .iter()
        .cloned()
        .partition(|r| now_ms.saturating_sub(r.since_ms) >= RETIRE_MS);
    keep.extend(new.into_iter().map(|name| Retired {
        name,
        since_ms: now_ms,
    }));
    let mut due: Vec<String> = due.into_iter().map(|r| r.name).collect();
    let builds: Vec<String> = keep
        .iter()
        .filter(|r| is_gen_dir(&r.name))
        .map(|r| r.name.clone())
        .collect();
    // the oldest builds past the cap go now, with what was retired inside them
    let excess = &builds[..builds.len().saturating_sub(RETIRED_BUILDS)];
    let within = |name: &str| excess.iter().any(|b| name.split('/').next() == Some(b));
    let (gone, keep): (Vec<Retired>, Vec<Retired>) =
        keep.into_iter().partition(|r| within(&r.name));
    due.extend(gone.into_iter().map(|r| r.name));
    (keep, due)
}

/// A name cleanup may touch: inside a build directory of this layout, and
/// nowhere else.
fn owned(name: &str) -> bool {
    let mut parts = name.split('/');
    let top = parts.next().unwrap_or("");
    is_gen_dir(top) && parts.all(|p| !p.is_empty() && p != "." && p != "..")
}

fn is_gen_dir(name: &str) -> bool {
    name.strip_prefix("g-")
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn remove(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(path).is_ok(),
        Ok(_) => fs::remove_file(path).is_ok(),
        Err(_) => false,
    }
}

fn age_ms(path: &Path, now_ms: u64) -> u64 {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| now_ms.saturating_sub(d.as_millis() as u64))
        .unwrap_or(0)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Remove `due` (retired entries past their grace period) and, with
/// `orphans`, entries of `dir` that `m` does not name and that are older
/// than [`ORPHAN_MS`]: other build directories, spill directories of
/// processes that are gone, and temp files. A build directory's own leftovers
/// go with it once it is retired. Only names this layout owns, at most
/// [`MAX_REMOVALS`]. Callers hold the writer lock and have published `m`.
pub fn clean(dir: &Path, m: &Manifest, due: &[String], orphans: bool, now_ms: u64) -> usize {
    let mut removed = 0;
    for name in due.iter().filter(|n| owned(n)) {
        if removed >= MAX_REMOVALS {
            return removed;
        }
        removed += usize::from(remove(&dir.join(name)));
    }
    if !orphans {
        return removed;
    }
    let current = gen_dir(m.epoch);
    let retired = |name: &str| m.retired.iter().any(|r| r.name == name);
    let old = |p: &Path| age_ms(p, now_ms) > ORPHAN_MS;
    let mut victims = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        let orphan = if is_gen_dir(&name) {
            name != current && !retired(&name) && old(&p)
        } else if let Some(pid) = name.strip_prefix("scratch-") {
            pid.parse().is_ok_and(|pid: i32| !alive(pid)) && old(&p)
        } else {
            name.ends_with(".tmp") && old(&p)
        };
        if orphan {
            victims.push(p);
        }
    }
    for p in victims {
        if removed >= MAX_REMOVALS {
            break;
        }
        removed += usize::from(remove(&p));
    }
    removed
}

/// Remove every retired entry now, as if its grace period were over: for
/// tests of what a reader keeps once the files are gone.
#[doc(hidden)]
pub fn expire_retired(dir: &Path) -> usize {
    let Some(m) = crate::read_manifest(dir) else {
        return 0;
    };
    let due: Vec<String> = m.retired.iter().map(|r| r.name.clone()).collect();
    clean(dir, &m, &due, false, crate::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_entries_leave_after_their_grace_period() {
        let prev = vec![
            Retired {
                name: "g-0000000000000001".into(),
                since_ms: 1_000,
            },
            Retired {
                name: "g-0000000000000002/files.1.bin".into(),
                since_ms: 20_000,
            },
        ];
        let (keep, due) = retire(&prev, ["g-0000000000000003".to_string()], 31_000);
        assert_eq!(due, ["g-0000000000000001"]);
        assert_eq!(
            keep.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["g-0000000000000002/files.1.bin", "g-0000000000000003"]
        );
    }

    #[test]
    fn only_the_newest_retired_builds_wait_out_their_grace_period() {
        let mut retired = Vec::new();
        for e in 1..=4u64 {
            let new = [gen_dir(e), format!("{}/files.1.bin", gen_dir(e))];
            let (keep, due) = retire(&retired, new, 1_000);
            retired = keep;
            if e == 4 {
                assert_eq!(due, [gen_dir(2), format!("{}/files.1.bin", gen_dir(2))]);
            }
        }
        let names: Vec<&str> = retired.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "g-0000000000000003",
                "g-0000000000000003/files.1.bin",
                "g-0000000000000004",
                "g-0000000000000004/files.1.bin",
            ]
        );
    }

    #[test]
    fn cleanup_touches_only_names_this_layout_owns() {
        assert!(owned("g-00000000000000ab"));
        assert!(owned("g-00000000000000ab/d-0001.bin"));
        for bad in [
            "manifest",
            "LOCK",
            "../x",
            "g-00000000000000ab/../x",
            "g-12/x",
            "session",
        ] {
            assert!(!owned(bad), "{bad}");
        }
    }
}
