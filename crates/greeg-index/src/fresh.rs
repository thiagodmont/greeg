//! Freshness (DESIGN.md §4.5): find what changed since the index was
//! published (stat pass, or FSEvents history on macOS), and apply changes as
//! a delta segment plus tombstones.

use crate::build::{WalkedDir, WalkedFile, build_delta};
use crate::format::{self};
use crate::{Index, now_ms, write_manifest};
use anyhow::Result;
use hashbrown::{HashMap, HashSet};
use roaring::RoaringBitmap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    None,
    Stat,
    FsEvents,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "auto" => Mode::Auto,
            "none" => Mode::None,
            "stat" => Mode::Stat,
            "fsevents" => Mode::FsEvents,
            _ => return None,
        })
    }
}

#[derive(Debug, Default)]
pub struct Changes {
    pub modified: Vec<(u32, WalkedFile)>,
    pub deleted: Vec<u32>,
    pub added: Vec<WalkedFile>,
    pub added_dirs: Vec<WalkedDir>,
    /// existing dirs whose mtime moved (recorded so the next check is quiet)
    pub touched_dirs: Vec<WalkedDir>,
    pub ignore_changed: bool,
    pub method: &'static str,
    pub ms: f64,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.added.is_empty() && self.added_dirs.is_empty() && self.touched_dirs.is_empty()
    }
    pub fn count(&self) -> usize {
        self.modified.len() + self.added.len()
    }
}

fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.mtime() * 1_000_000_000 + md.mtime_nsec()
}

/// Skip the check when the index was verified this recently. Kept short:
/// an agent never edits and searches within 100 ms, but scripts can.
pub const TTL_MS: u64 = 100;
pub const FSEVENTS_CUTOFF: Duration = Duration::from_millis(150);

struct Known<'a> {
    // (id, rel, size, mtime, dir)
    files: Vec<(u32, &'a str, u64, i64, u32)>,
    // dir rel -> mtime (latest record wins)
    dirs: HashMap<&'a str, i64>,
}

impl<'a> Known<'a> {
    /// Names of known children (files and dirs) of `dir`, computed on demand.
    fn children_of(&self, dir: &str) -> HashSet<&'a str> {
        let mut out = HashSet::new();
        for (_, rel, _, _, _) in &self.files {
            let (d, n) = rel.rsplit_once('/').unwrap_or(("", rel));
            if d == dir {
                out.insert(n);
            }
        }
        for d in self.dirs.keys() {
            let (parent, name) = d.rsplit_once('/').unwrap_or(("", d));
            if parent == dir && !name.is_empty() {
                out.insert(name);
            }
        }
        out
    }
}

fn known(idx: &Index) -> Known<'_> {
    let mut files = Vec::with_capacity(idx.base.n_files as usize);
    for (id, rel, rec) in idx.live_files() {
        files.push((id, rel, rec.size, rec.mtime_ns, rec.dir));
    }
    let mut dirs: HashMap<&str, i64> = HashMap::with_capacity(idx.base.files().dirs.len());
    let base = idx.base.files();
    for d in base.dirs {
        dirs.insert(base.dir_path(d), d.mtime_ns);
    }
    for seg in &idx.deltas {
        let fv = seg.files();
        for d in fv.dirs {
            dirs.insert(fv.dir_path(d), d.mtime_ns);
        }
    }
    Known { files, dirs }
}

/// Parallel lstat of `items`, returning per item Some(size, mtime) or None if missing.
fn stat_many<T: Sync>(root: &Path, items: &[T], rel: impl Fn(&T) -> &str + Sync, threads: usize) -> Vec<Option<(u64, i64)>> {
    let n = items.len();
    let mut out: Vec<Option<(u64, i64)>> = vec![None; n];
    if n == 0 {
        return out;
    }
    let threads = threads.clamp(1, 8).min(n);
    let chunk = n.div_ceil(threads);
    std::thread::scope(|sc| {
        for (i, part) in out.chunks_mut(chunk).enumerate() {
            let items = &items[i * chunk..(i * chunk + part.len())];
            let rel = &rel;
            sc.spawn(move || {
                for (o, it) in part.iter_mut().zip(items) {
                    *o = fs::symlink_metadata(root.join(rel(it))).ok().map(|md| (md.len(), mtime_ns(&md)));
                }
            });
        }
    });
    out
}

/// List a directory with ignore rules (one level), returning (name, is_dir, size, mtime).
fn list_dir(root: &Path, rel: &str) -> Vec<(String, bool, u64, i64)> {
    let abs = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let mut out = Vec::new();
    for e in ignore::WalkBuilder::new(&abs).hidden(true).max_depth(Some(1)).parents(true).build().flatten() {
        if e.depth() == 0 {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((name, is_dir, md.len(), mtime_ns(&md)));
    }
    out
}

/// Recursively walk a new directory (ignore rules honoured) into added files/dirs.
fn walk_new_dir(root: &Path, rel: &str, dir_id: u32, ch: &mut Changes) {
    let abs = root.join(rel);
    for e in ignore::WalkBuilder::new(&abs).hidden(true).parents(true).build().flatten() {
        let Ok(md) = e.metadata() else { continue };
        let r = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            ch.added_dirs.push(WalkedDir { rel: r, mtime_ns: mtime_ns(&md) });
        } else if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            ch.added.push(WalkedFile { rel: r, size: md.len(), mtime_ns: mtime_ns(&md), dir: dir_id });
        }
    }
}

fn is_ignore_file(rel: &str) -> bool {
    let name = rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel);
    matches!(name, ".gitignore" | ".ignore" | ".rgignore")
}

/// Full stat pass.
pub fn check_stat(idx: &Index, root: &Path, threads: usize) -> Changes {
    let t = Instant::now();
    let k = known(idx);
    let mut ch = Changes { method: "stat", ..Default::default() };
    let st = stat_many(root, &k.files, |f| f.1, threads);
    for ((id, rel, size, mt, dir), s) in k.files.iter().zip(st) {
        match s {
            None => ch.deleted.push(*id),
            Some((sz, m)) if sz != *size || m != *mt => {
                if is_ignore_file(rel) {
                    ch.ignore_changed = true;
                }
                ch.modified.push((*id, WalkedFile { rel: rel.to_string(), size: sz, mtime_ns: m, dir: *dir }));
            }
            _ => {}
        }
    }
    let dirs: Vec<(&str, i64)> = k.dirs.iter().map(|(p, m)| (*p, *m)).collect();
    let ds = stat_many(root, &dirs, |d| d.0, threads);
    let mut changed_dirs: Vec<(String, i64)> = Vec::new();
    for ((rel, mt), s) in dirs.iter().zip(ds) {
        match s {
            None => {}
            Some((_, m)) if m != *mt => changed_dirs.push((rel.to_string(), m)),
            _ => {}
        }
    }
    relist_dirs(root, &k, &changed_dirs, &mut ch);
    ch.ms = t.elapsed().as_secs_f64() * 1e3;
    ch
}

/// Re-list changed directories to find additions (files and subdirectories).
fn relist_dirs(root: &Path, k: &Known, changed_dirs: &[(String, i64)], ch: &mut Changes) {
    if changed_dirs.is_empty() {
        return;
    }
    let dir_ids: HashMap<&str, u32> = k.files.iter().map(|f| (f.1.rsplit_once('/').map(|(d, _)| d).unwrap_or(""), f.4)).collect();
    for (rel, new_mtime) in changed_dirs {
        ch.touched_dirs.push(WalkedDir { rel: rel.clone(), mtime_ns: *new_mtime });
        let kids = k.children_of(rel);
        let dir_id = *dir_ids.get(rel.as_str()).unwrap_or(&0);
        for (name, is_dir, size, mt) in list_dir(root, rel) {
            if kids.contains(&name[..]) {
                continue;
            }
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if is_dir {
                walk_new_dir(root, &child_rel, dir_id, ch);
            } else {
                if is_ignore_file(&child_rel) {
                    ch.ignore_changed = true;
                }
                ch.added.push(WalkedFile { rel: child_rel, size, mtime_ns: mt, dir: dir_id });
            }
        }
    }
}

/// FSEvents-scoped check (macOS). Returns None when the log is unusable.
pub fn check_fsevents(idx: &Index, root: &Path, threads: usize) -> Option<Changes> {
    if idx.manifest.fsevents_id == 0 {
        return None;
    }
    let t = Instant::now();
    let abs_root = fs::canonicalize(root).ok()?;
    let root_s = abs_root.to_string_lossy().into_owned();
    let dirs = greeg_fsevents::changed_dirs_since(idx.manifest.fsevents_id, &root_s, FSEVENTS_CUTOFF)?;
    let mut ch = Changes { method: "fsevents", ..Default::default() };
    if dirs.is_empty() {
        ch.ms = t.elapsed().as_secs_f64() * 1e3;
        return Some(ch);
    }
    let k = known(idx);
    // relative dir set
    let mut rels: HashSet<String> = HashSet::new();
    for d in dirs {
        let d = d.trim_end_matches('/');
        let r = if d.len() > root_s.len() { d[root_s.len()..].trim_start_matches('/').to_string() } else { String::new() };
        // events on paths outside the index (ignored dirs) still matter when they are
        // new: their parent appears too, so only known dirs and the root are listed.
        rels.insert(r);
    }
    // stat files whose parent dir changed
    let files: Vec<&(u32, &str, u64, i64, u32)> = k.files.iter().filter(|f| rels.contains(f.1.rsplit_once('/').map(|(d, _)| d).unwrap_or(""))).collect();
    let st = stat_many(root, &files, |f| f.1, threads);
    for (f, s) in files.iter().zip(st) {
        match s {
            None => ch.deleted.push(f.0),
            Some((sz, m)) if sz != f.2 || m != f.3 => {
                if is_ignore_file(f.1) {
                    ch.ignore_changed = true;
                }
                ch.modified.push((f.0, WalkedFile { rel: f.1.to_string(), size: sz, mtime_ns: m, dir: f.4 }));
            }
            _ => {}
        }
    }
    // re-list changed known dirs
    let mut changed_dirs = Vec::new();
    for r in &rels {
        if let Some(&old) = k.dirs.get(r.as_str())
            && let Ok(md) = fs::symlink_metadata(if r.is_empty() { abs_root.clone() } else { abs_root.join(r) }) {
                let m = mtime_ns(&md);
                if m != old {
                    changed_dirs.push((r.clone(), m));
                }
            }
    }
    relist_dirs(root, &k, &changed_dirs, &mut ch);
    ch.ms = t.elapsed().as_secs_f64() * 1e3;
    Some(ch)
}

/// Decide and run the check for `mode`. `threads` is the stat pool size.
pub fn check(idx: &Index, root: &Path, mode: Mode, threads: usize) -> Option<Changes> {
    match mode {
        Mode::None => None,
        Mode::Stat => Some(check_stat(idx, root, threads)),
        Mode::FsEvents => check_fsevents(idx, root, threads).or_else(|| Some(check_stat(idx, root, threads))),
        Mode::Auto => {
            if now_ms().saturating_sub(idx.manifest.verified_unix_ms) < TTL_MS {
                return None;
            }
            // FSEvents costs ~10 ms of stream setup regardless of tree size; the
            // stat pass is cheaper below a few thousand files (measured 0.8 ms at 849).
            if cfg!(target_os = "macos") && idx.base.n_files >= 8000
                && let Some(c) = check_fsevents(idx, root, threads) {
                    return Some(c);
                }
            Some(check_stat(idx, root, threads))
        }
    }
}

/// Apply changes as a new delta segment; update the manifest. Returns the
/// number of files re-extracted.
pub fn apply(idx: &Index, root: &Path, ch: &Changes) -> Result<usize> {
    let mut m = idx.manifest.clone();
    let fsid = current_fsevents_id();
    if ch.is_empty() {
        m.verified_unix_ms = now_ms();
        if fsid != 0 {
            m.fsevents_id = fsid;
        }
        write_manifest(&idx.dir, &m)?;
        return Ok(0);
    }
    let first_id = idx.next_id();
    let mut files: Vec<WalkedFile> = Vec::with_capacity(ch.modified.len() + ch.added.len());
    let mut tomb = RoaringBitmap::new();
    for (id, w) in &ch.modified {
        tomb.insert(*id);
        files.push(WalkedFile { rel: w.rel.clone(), size: w.size, mtime_ns: w.mtime_ns, dir: w.dir });
    }
    for id in &ch.deleted {
        tomb.insert(*id);
    }
    for w in &ch.added {
        files.push(WalkedFile { rel: w.rel.clone(), size: w.size, mtime_ns: w.mtime_ns, dir: w.dir });
    }
    let mut dirs: Vec<WalkedDir> = ch.added_dirs.iter().map(|d| WalkedDir { rel: d.rel.clone(), mtime_ns: d.mtime_ns }).collect();
    dirs.extend(ch.touched_dirs.iter().map(|d| WalkedDir { rel: d.rel.clone(), mtime_ns: d.mtime_ns }));
    let body = build_delta(root, first_id, &files, &dirs, &tomb)?;
    let ddir = idx.dir.join("delta");
    fs::create_dir_all(&ddir)?;
    let n = idx.deltas.len() as u32 + 1;
    format::write_atomic(&ddir.join(format!("{n:04}.bin")), format::COMP_DELTA, &body)?;
    m.deltas = n;
    m.tombstones += tomb.len() as u32;
    m.verified_unix_ms = now_ms();
    if fsid != 0 {
        m.fsevents_id = fsid;
    }
    write_manifest(&idx.dir, &m)?;
    Ok(files.len())
}

fn current_fsevents_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        greeg_fsevents::current_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Should a full rebuild be spawned instead of applying inline?
pub fn needs_rebuild(idx: &Index, ch: &Changes) -> bool {
    ch.ignore_changed || ch.count() > 2000 || idx.deltas.len() >= 16 || (idx.tomb.len() + ch.count() as u64) * 20 > idx.base.n_files as u64
}
