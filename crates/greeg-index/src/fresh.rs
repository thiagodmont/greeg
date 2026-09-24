//! Freshness (ARCHITECTURE.md): find what changed since the index was
//! published (stat pass, or FSEvents history on macOS), and apply changes as
//! a delta segment plus tombstones.

use crate::build::{Noted, Stamp, WalkedDir, WalkedFile, build_delta, walker_noting};
use crate::format::{self, FileRec, NONE, is_ignore_file};
use crate::rel::{self, as_path, parent as parent_of};
use crate::skipped;
use crate::{Index, now_ms, read_manifest, write_manifest};
use anyhow::Result;
use hashbrown::{HashMap, HashSet};
use roaring::RoaringBitmap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
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
    /// The skipped children of every directory listed again (`skipped.rs`).
    pub skipped: Vec<(Vec<u8>, skipped::Children)>,
    /// An ignore file was added, modified or deleted: the ignore rules of its
    /// subtree must be re-evaluated (full rebuild; `needs_rebuild`).
    pub ignore_changed: bool,
    pub method: &'static str,
    pub ms: f64,
    /// FSEvents id captured *before* the check ran, so edits made while the
    /// check was running are replayed by the next check (C15).
    pub fsevents_id: u64,
    /// This check found that the file system does not keep inode numbers
    /// (`classify_all`); `apply` records it in the manifest.
    pub no_ino: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty()
            && self.deleted.is_empty()
            && self.added.is_empty()
            && self.added_dirs.is_empty()
            && self.touched_dirs.is_empty()
            && self.skipped.is_empty()
    }
    pub fn count(&self) -> usize {
        self.modified.len() + self.added.len()
    }
}

/// Skip the check when the index was verified this recently. Kept short:
/// an agent never edits and searches within 100 ms, but scripts can.
pub const TTL_MS: u64 = 100;
/// FSEvents normally reports a 60–75k-file tree in 9–15 ms, but the daemon is intermittently slow
/// (135 ms+ in roughly 1 run in 10–30 on the reference machine, cause unknown); past this cutoff
/// the check falls through to the stat pass (≈ 41 ms on 66k files), bounding the worst case at
/// about 80 ms instead of 150 ms plus a stat pass.
pub const FSEVENTS_CUTOFF: Duration = Duration::from_millis(40);
/// A check that found nothing rewrites `verified_unix_ms` at most this often.
pub const VERIFY_WRITE_MS: u64 = 1000;

struct Known<'a> {
    // (id, rel, rec)
    files: Vec<(u32, &'a [u8], &'a FileRec)>,
    // dir rel -> stamp (latest record wins)
    dirs: HashMap<&'a [u8], Stamp>,
}

impl<'a> Known<'a> {
    /// Known child names (files and dirs) of each directory in `dirs`, in one
    /// pass over the file table.
    fn children_of<'d>(
        &self,
        dirs: &'d [(Vec<u8>, Stamp)],
    ) -> HashMap<&'d [u8], HashSet<&'a [u8]>> {
        let mut out: HashMap<&'d [u8], HashSet<&'a [u8]>> = dirs
            .iter()
            .map(|(d, _)| (d.as_slice(), HashSet::new()))
            .collect();
        for (_, r, _) in &self.files {
            if let Some(set) = out.get_mut(parent_of(r)) {
                set.insert(rel::file_name(r));
            }
        }
        for d in self.dirs.keys() {
            let name = rel::file_name(d);
            if !name.is_empty()
                && let Some(set) = out.get_mut(parent_of(d))
            {
                set.insert(name);
            }
        }
        out
    }
}

fn known(idx: &Index) -> Known<'_> {
    let mut files = Vec::with_capacity(idx.base.n_files as usize);
    files.extend(idx.tracked_files());
    let mut dirs: HashMap<&[u8], Stamp> = HashMap::with_capacity(idx.base.files().dirs.len());
    for (_, seg) in idx.segments() {
        let fv = seg.files();
        for d in fv.dirs {
            dirs.insert(fv.dir_path(d), d.stamp());
        }
    }
    Known { files, dirs }
}

/// Parallel lstat of `items`, returning per item its stamp, or None when
/// missing or no longer of the `kind` the index holds (a regular file
/// replaced by a symlink reads as deleted, as a scan would skip it).
fn stat_many<T: Sync>(
    root: &Path,
    items: &[T],
    rel: impl Fn(&T) -> &[u8] + Sync,
    kind: fn(&fs::FileType) -> bool,
    threads: usize,
) -> Vec<Option<Stamp>> {
    let n = items.len();
    let mut out: Vec<Option<Stamp>> = vec![None; n];
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
                    *o = fs::symlink_metadata(root.join(as_path(rel(it))))
                        .ok()
                        .filter(|md| kind(&md.file_type()))
                        .map(|md| Stamp::of(&md));
                }
            });
        }
    });
    out
}

/// List a directory with ignore rules (one level), returning (name, is_dir,
/// stamp) for subdirectories and regular files, and what it skipped.
fn list_dir(root: &Path, dir: &[u8]) -> (Vec<(Vec<u8>, bool, Stamp)>, skipped::Children) {
    let abs = if dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(as_path(dir))
    };
    let noted = Noted::default();
    let mut kept = HashSet::new();
    let mut out = Vec::new();
    for e in walker_noting(&abs, Some(noted.clone()))
        .max_depth(Some(1))
        .parents(true)
        .build()
        .flatten()
    {
        if e.depth() == 0 {
            continue;
        }
        kept.insert(rel::of(root, e.path()));
        let Some(ft) = e.file_type() else { continue };
        if !ft.is_dir() && !ft.is_file() {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let name = e.file_name().as_bytes().to_vec();
        out.push((name, ft.is_dir(), Stamp::of(&md)));
    }
    let noted = noted_rels(root, &noted);
    let kept: HashSet<&[u8]> = kept.iter().map(Vec::as_slice).collect();
    let hidden: HashSet<&[u8]> = noted.iter().map(Vec::as_slice).collect();
    let skipped = skipped::list(root, &[dir], &kept, &hidden)
        .pop()
        .map(|(_, kids)| kids)
        .unwrap_or_default();
    (out, skipped)
}

fn noted_rels(root: &Path, noted: &Noted) -> Vec<Vec<u8>> {
    noted
        .lock()
        .unwrap()
        .iter()
        .map(|p| rel::of(root, p))
        .collect()
}

/// Recursively walk a new directory (ignore rules honoured) into added files/dirs.
fn walk_new_dir(root: &Path, dir: &[u8], dir_id: u32, ch: &mut Changes) {
    let abs = root.join(as_path(dir));
    let noted = Noted::default();
    let mut kept = HashSet::new();
    let mut walked = Vec::new();
    for e in walker_noting(&abs, Some(noted.clone()))
        .parents(true)
        .build()
        .flatten()
    {
        let Ok(md) = e.metadata() else { continue };
        let r = rel::of(root, e.path());
        kept.insert(r.clone());
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walked.push(r.clone());
            ch.added_dirs.push(WalkedDir {
                rel: r,
                stamp: Stamp::of(&md),
            });
        } else if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            if is_ignore_file(&r) {
                ch.ignore_changed = true;
            }
            ch.added.push(WalkedFile {
                rel: r,
                stamp: Stamp::of(&md),
                dir: dir_id,
            });
        }
    }
    let noted = noted_rels(root, &noted);
    let kept: HashSet<&[u8]> = kept.iter().map(Vec::as_slice).collect();
    let hidden: HashSet<&[u8]> = noted.iter().map(Vec::as_slice).collect();
    let dirs: Vec<&[u8]> = walked.iter().map(Vec::as_slice).collect();
    ch.skipped
        .extend(skipped::list(root, &dirs, &kept, &hidden));
}

/// Record the stat outcome of one known file. A record marked for a recheck
/// counts as modified whatever its stamp says.
fn classify(ch: &mut Changes, id: u32, rel: &[u8], rec: &FileRec, s: Option<Stamp>, no_ino: bool) {
    match s {
        None => {
            if is_ignore_file(rel) {
                ch.ignore_changed = true;
            }
            ch.deleted.push(id);
        }
        Some(st)
            if !st.same(&rec.stamp(), no_ino) || rec.stamp_flags & format::STAMP_RECHECK != 0 =>
        {
            if is_ignore_file(rel) {
                ch.ignore_changed = true;
            }
            ch.modified.push((
                id,
                WalkedFile {
                    rel: rel.to_vec(),
                    stamp: st,
                    dir: rec.dir,
                },
            ));
        }
        _ => {}
    }
}

/// Stat'ed files needed before a check concludes that the file system does
/// not keep inode numbers.
const NO_INO_MIN: usize = 16;

/// Classify every stat'ed file. When most of the `indexed` files differ from
/// their records only by inode or device, the file system does not keep inode numbers (some
/// network and FUSE mounts): the check stops comparing them rather than
/// re-extract the tree every time, and `apply` records `stamp_mode = "no-ino"`.
fn classify_all(
    ch: &mut Changes,
    mut no_ino: bool,
    files: &[(u32, &[u8], &FileRec)],
    st: Vec<Option<Stamp>>,
    indexed: usize,
) -> bool {
    if !no_ino {
        let moved = files
            .iter()
            .zip(&st)
            .filter(|(f, s)| s.is_some_and(|s| s.moved_only(&f.2.stamp())))
            .count();
        if moved >= NO_INO_MIN && moved * 2 > indexed {
            no_ino = true;
            ch.no_ino = true;
        }
    }
    for ((id, rel, rec), s) in files.iter().zip(st) {
        classify(ch, *id, rel, rec, s, no_ino);
    }
    no_ino
}

/// Full stat pass.
pub fn check_stat(idx: &Index, root: &Path, threads: usize) -> Changes {
    let t = Instant::now();
    let fsevents_id = current_fsevents_id();
    let k = known(idx);
    let mut ch = Changes {
        method: "stat",
        fsevents_id,
        ..Default::default()
    };
    let st = stat_many(root, &k.files, |f| f.1, fs::FileType::is_file, threads);
    let no_ino = classify_all(&mut ch, idx.no_ino(), &k.files, st, k.files.len());
    let dirs: Vec<(&[u8], Stamp)> = k.dirs.iter().map(|(p, m)| (*p, *m)).collect();
    let ds = stat_many(root, &dirs, |d| d.0, fs::FileType::is_dir, threads);
    let mut changed_dirs: Vec<(Vec<u8>, Stamp)> = Vec::new();
    for ((rel, old), s) in dirs.iter().zip(ds) {
        match s {
            None => {}
            Some(st) if !st.same_dir(old, no_ino) => changed_dirs.push((rel.to_vec(), st)),
            _ => {}
        }
    }
    relist_dirs(root, &k, &changed_dirs, &mut ch);
    ch.ms = t.elapsed().as_secs_f64() * 1e3;
    ch
}

/// Re-list changed directories to find additions (files and subdirectories).
fn relist_dirs(root: &Path, k: &Known, changed_dirs: &[(Vec<u8>, Stamp)], ch: &mut Changes) {
    if changed_dirs.is_empty() {
        return;
    }
    let kids = k.children_of(changed_dirs);
    let dir_ids: HashMap<&[u8], u32> = k.files.iter().map(|f| (parent_of(f.1), f.2.dir)).collect();
    for (dir, new_stamp) in changed_dirs {
        ch.touched_dirs.push(WalkedDir {
            rel: dir.clone(),
            stamp: *new_stamp,
        });
        let known_kids = kids.get(dir.as_slice());
        let dir_id = *dir_ids.get(dir.as_slice()).unwrap_or(&0);
        let (listed, skipped) = list_dir(root, dir);
        ch.skipped.push((dir.clone(), skipped));
        for (name, is_dir, stamp) in listed {
            if known_kids.map(|s| s.contains(&name[..])).unwrap_or(false) {
                continue;
            }
            let child_rel = rel::join(dir, &name);
            if is_dir {
                walk_new_dir(root, &child_rel, dir_id, ch);
            } else {
                if is_ignore_file(&child_rel) {
                    ch.ignore_changed = true;
                }
                ch.added.push(WalkedFile {
                    rel: child_rel,
                    stamp,
                    dir: dir_id,
                });
            }
        }
    }
}

/// FSEvents-scoped check (macOS). Returns None when the log is unusable.
#[cfg(not(target_os = "macos"))]
pub fn check_fsevents(_idx: &Index, _root: &Path, _threads: usize) -> Option<Changes> {
    None
}

/// FSEvents-scoped check (macOS). Returns None when the log is unusable.
#[cfg(target_os = "macos")]
pub fn check_fsevents(idx: &Index, root: &Path, threads: usize) -> Option<Changes> {
    if idx.manifest.fsevents_id == 0 {
        return None;
    }
    let t = Instant::now();
    let fsevents_id = current_fsevents_id();
    let abs_root = fs::canonicalize(root).ok()?;
    let root_s = abs_root.to_string_lossy().into_owned();
    let dirs =
        greeg_fsevents::changed_dirs_since(idx.manifest.fsevents_id, &root_s, FSEVENTS_CUTOFF)?;
    let mut ch = Changes {
        method: "fsevents",
        fsevents_id,
        ..Default::default()
    };
    if dirs.is_empty() {
        ch.ms = t.elapsed().as_secs_f64() * 1e3;
        return Some(ch);
    }
    let k = known(idx);
    // relative dir set
    let mut rels: HashSet<Vec<u8>> = HashSet::new();
    for d in dirs {
        let d = d.trim_end_matches('/');
        let r = if d.len() > root_s.len() {
            d[root_s.len()..].trim_start_matches('/').to_string()
        } else {
            String::new()
        };
        // events on paths outside the index (ignored dirs) still matter when they are
        // new: their parent appears too, so only known dirs and the root are listed.
        rels.insert(r.into_bytes());
    }
    // known dirs whose entry list moved: a rename or removal of a subdirectory
    // reports only the parent, so every known file below it is re-stat'd too
    let mut changed_dirs = Vec::new();
    let mut moved: HashSet<&[u8]> = HashSet::new();
    for r in &rels {
        if let Some(old) = k.dirs.get(r.as_slice())
            && let Ok(md) = fs::symlink_metadata(if r.is_empty() {
                abs_root.clone()
            } else {
                abs_root.join(as_path(r))
            })
        {
            let st = Stamp::of(&md);
            if !st.same_dir(old, idx.no_ino()) {
                changed_dirs.push((r.clone(), st));
                moved.insert(r.as_slice());
            }
        }
    }
    let under_moved = |rel: &[u8]| -> bool {
        if moved.is_empty() {
            return false;
        }
        if moved.contains(&b""[..]) {
            return true;
        }
        let mut end = rel.len();
        while let Some(k) = rel[..end].iter().rposition(|&b| b == b'/') {
            if moved.contains(&rel[..k]) {
                return true;
            }
            end = k;
        }
        false
    };
    // stat files whose parent dir changed, or that live below a dir whose entries moved
    let files: Vec<(u32, &[u8], &FileRec)> = k
        .files
        .iter()
        .filter(|f| rels.contains(parent_of(f.1)) || under_moved(f.1))
        .copied()
        .collect();
    let st = stat_many(root, &files, |f| f.1, fs::FileType::is_file, threads);
    classify_all(&mut ch, idx.no_ino(), &files, st, k.files.len());
    relist_dirs(root, &k, &changed_dirs, &mut ch);
    ch.ms = t.elapsed().as_secs_f64() * 1e3;
    Some(ch)
}

/// The check to run when `Auto` must not trust the TTL stamp: a retry after
/// another writer published, or the detached refresh of an answer-first
/// query. FSEvents on large macOS trees, else the stat pass.
pub fn explicit_mode(idx: &Index) -> Mode {
    if cfg!(target_os = "macos") && idx.base.n_files >= 8000 {
        Mode::FsEvents
    } else {
        Mode::Stat
    }
}

/// Decide and run the check for `mode`. `threads` is the stat pool size.
pub fn check(idx: &Index, root: &Path, mode: Mode, threads: usize) -> Option<Changes> {
    let mut ch = check_tree(idx, root, mode, threads)?;
    let recorded = &idx.manifest.ignore_inputs;
    if !recorded.is_empty() && *recorded != crate::ignores::digest(root) {
        ch.ignore_changed = true;
    }
    Some(ch)
}

fn check_tree(idx: &Index, root: &Path, mode: Mode, threads: usize) -> Option<Changes> {
    match mode {
        Mode::None => None,
        Mode::Stat => Some(check_stat(idx, root, threads)),
        Mode::FsEvents => {
            check_fsevents(idx, root, threads).or_else(|| Some(check_stat(idx, root, threads)))
        }
        Mode::Auto => {
            if now_ms().saturating_sub(idx.manifest.verified_unix_ms) < TTL_MS {
                return None;
            }
            // FSEvents costs ~10 ms of stream setup regardless of tree size; the
            // stat pass is cheaper below a few thousand files (measured 0.8 ms at 849).
            if cfg!(target_os = "macos")
                && idx.base.n_files >= 8000
                && let Some(c) = check_fsevents(idx, root, threads)
            {
                return Some(c);
            }
            Some(check_stat(idx, root, threads))
        }
    }
}

/// Does the on-disk manifest still describe the index `idx` was opened from?
fn same_index(idx: &Index, cur: &crate::Manifest) -> bool {
    let m = &idx.manifest;
    cur.generation == m.generation
        && cur.phase1 == m.phase1
        && cur.phase2 == m.phase2
        && cur.deltas == m.deltas
}

/// Apply changes as a new delta segment; update the manifest. Returns the
/// number of files re-extracted. Runs under the writer lock and re-reads the
/// manifest first: if a build or another query republished since `idx` was
/// opened, the delta (computed against stale ids) is skipped and 0 is
/// returned; the caller reopens the index either way.
pub fn apply(idx: &Index, root: &Path, ch: &Changes) -> Result<usize> {
    let fsid = if ch.fsevents_id != 0 {
        ch.fsevents_id
    } else {
        current_fsevents_id()
    };
    if ch.is_empty() {
        // nothing to publish: refresh the TTL stamp, at most once per second
        if !ch.no_ino && now_ms().saturating_sub(idx.manifest.verified_unix_ms) < VERIFY_WRITE_MS {
            return Ok(0);
        }
        let _lock = crate::lock::writer(&idx.dir)?;
        let Some(mut m) = read_manifest(&idx.dir) else {
            return Ok(0);
        };
        if !same_index(idx, &m) {
            return Ok(0);
        }
        m.verified_unix_ms = now_ms();
        if fsid != 0 {
            m.fsevents_id = fsid;
        }
        if ch.no_ino {
            m.stamp_mode = crate::STAMP_NO_INO.into();
        }
        write_manifest(&idx.dir, &m)?;
        return Ok(0);
    }
    let first_id = idx.next_id();
    let mut files: Vec<WalkedFile> = Vec::with_capacity(ch.modified.len() + ch.added.len());
    let mut prev: Vec<u32> = Vec::with_capacity(files.capacity());
    let mut tomb = RoaringBitmap::new();
    for (id, w) in &ch.modified {
        tomb.insert(*id);
        prev.push(*id);
        files.push(w.clone());
    }
    for id in &ch.deleted {
        tomb.insert(*id);
    }
    for w in &ch.added {
        prev.push(NONE);
        files.push(w.clone());
    }
    let mut dirs: Vec<WalkedDir> = ch.added_dirs.clone();
    dirs.extend(ch.touched_dirs.iter().cloned());
    // extraction runs outside the lock; only the publish is serialized
    let body = build_delta(idx, root, first_id, &files, &prev, &dirs, &tomb)?;
    // an index without the record stays without it: coverage unknown
    let skipped = if ch.skipped.is_empty() {
        None
    } else {
        idx.skipped().and_then(|mut s| {
            let before = s.clone();
            s.update(&ch.skipped);
            (s != before).then_some(s)
        })
    };
    let _lock = crate::lock::writer(&idx.dir)?;
    let Some(mut m) = read_manifest(&idx.dir) else {
        return Ok(0);
    };
    if !same_index(idx, &m) {
        return Ok(0);
    }
    let ddir = idx.dir.join("delta");
    crate::create_private_dir(&ddir)?;
    let n = idx.deltas.len() as u32 + 1;
    // not fsynced: a torn delta fails `Index::open` and rebuilds, and the
    // F_FULLFSYNC was 4–5 ms of every post-edit query (M10)
    format::write_atomic_with(
        &ddir.join(format!("{n:04}.bin")),
        format::COMP_DELTA,
        &body,
        false,
    )?;
    if let Some(s) = skipped {
        let name = format!("delta/{n:04}.skipped");
        s.write(&idx.dir.join(&name))?;
        m.skipped = name;
    }
    m.deltas = n;
    m.tombstones += tomb.len() as u32;
    if ch.no_ino {
        m.stamp_mode = crate::STAMP_NO_INO.into();
    }
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
    ch.ignore_changed
        || ch.count() > 2000
        || idx.deltas.len() >= 16
        || (idx.tomb.len() + ch.count() as u64) * 20 > idx.base.n_files as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_only_changes_switch_to_no_ino_mode_only_when_most_files_show_them() {
        let stamp = |ino: u64| Stamp {
            size: 10,
            mtime_ns: 1_000,
            ctime_ns: 2_000,
            ino,
            dev: 1,
        };
        let rec = |ino: u64| FileRec {
            size: 10,
            mtime_ns: 1_000,
            ctime_ns: 2_000,
            ino,
            dev: 1,
            ..bytemuck::Zeroable::zeroed()
        };
        let recs: Vec<FileRec> = (0..20).map(rec).collect();
        let files: Vec<(u32, &[u8], &FileRec)> = recs
            .iter()
            .enumerate()
            .map(|(i, r)| (i as u32, &b"f"[..], r))
            .collect();

        // every inode moved and nothing else: inode numbers are not kept
        let mut ch = Changes::default();
        assert!(classify_all(
            &mut ch,
            false,
            &files,
            (0..20).map(|i| Some(stamp(i + 100))).collect(),
            20
        ));
        assert!(ch.no_ino && ch.modified.is_empty());

        // three atomic replaces among stable inodes are changes
        let mut ch = Changes::default();
        let st = (0..20)
            .map(|i| Some(stamp(if i < 3 { i + 100 } else { i })))
            .collect();
        assert!(!classify_all(&mut ch, false, &files, st, 20));
        assert!(!ch.no_ino);
        assert_eq!(
            ch.modified.iter().map(|m| m.0).collect::<Vec<_>>(),
            [0, 1, 2]
        );

        // a subset of the index, as FSEvents stats, is weighed against the
        // whole of it: moved inodes there are replacements
        let mut ch = Changes::default();
        assert!(!classify_all(
            &mut ch,
            false,
            &files,
            (0..20).map(|i| Some(stamp(i + 100))).collect(),
            100
        ));
        assert!(!ch.no_ino && ch.modified.len() == 20);

        // a recheck mark is a change even with an identical stamp
        let mut marked = recs.clone();
        marked[5].stamp_flags = format::STAMP_RECHECK;
        let files: Vec<(u32, &[u8], &FileRec)> = marked
            .iter()
            .enumerate()
            .map(|(i, r)| (i as u32, &b"f"[..], r))
            .collect();
        let mut ch = Changes::default();
        classify_all(
            &mut ch,
            false,
            &files,
            (0..20).map(|i| Some(stamp(i))).collect(),
            20,
        );
        assert_eq!(ch.modified.iter().map(|m| m.0).collect::<Vec<_>>(), [5]);
    }
    use crate::build::{BuildOpts, build};
    use crate::format::NONE;

    fn tree(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("greeg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = base.join("index");
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        (base, root, dir)
    }

    #[test]
    fn a_reader_keeps_its_skipped_record_across_a_rebuild() {
        let (base, root, dir) = tree("skipped-rebuild");
        fs::write(root.join(".hidden.rs"), "fn h() {}\n").unwrap();
        fs::write(root.join("pkg/a.rs"), "fn a() {}\n").unwrap();
        let opts = BuildOpts {
            reader_threads: 1,
            quiet: true,
            phase1_only: true,
            ..Default::default()
        };
        build(&root, &dir, &opts).unwrap();
        let before = Index::open(&dir).unwrap();
        // the rebuild removes the previous generation's record
        build(&root, &dir, &opts).unwrap();
        assert!(!dir.join(&before.manifest.skipped).exists());
        let sk = before.skipped().expect("record kept by the open index");
        assert!(sk.entries().any(|(rel, _)| rel == b".hidden.rs"));
        let _ = fs::remove_dir_all(&base);
    }

    fn id_of(idx: &Index, rel: &str) -> u32 {
        let rel = rel.as_bytes();
        idx.live_files()
            .find(|(_, r, _)| *r == rel)
            .map(|(id, _, _)| id)
            .unwrap_or_else(|| panic!("{rel:?} not live"))
    }

    fn edit_and_apply(idx: &Index, root: &Path, dir: &Path) -> Index {
        let ch = check_stat(idx, root, 1);
        assert!(!ch.is_empty());
        assert!(!needs_rebuild(idx, &ch));
        assert!(apply(idx, root, &ch).unwrap() > 0);
        Index::open(dir).unwrap()
    }

    /// Edited files keep their rank and their import edges in both directions
    /// (ARCHITECTURE.md): the delta resolves imports against the base, and the
    /// base's edges to the superseded id follow the file to its new id.
    #[test]
    fn delta_keeps_rank_and_edges() {
        let (base, root, dir) = tree("delta-graph");
        fs::write(root.join("pkg/__init__.py"), "").unwrap();
        fs::write(
            root.join("pkg/a.py"),
            "from pkg.b import helper\n\ndef use():\n    helper()\n",
        )
        .unwrap();
        fs::write(root.join("pkg/b.py"), "def helper():\n    pass\n").unwrap();
        fs::write(root.join("pkg/c.py"), "from pkg.a import use\n").unwrap();
        // enough files that a few edits stay below the inline-apply threshold
        for i in 0..60 {
            fs::write(
                root.join(format!("pkg/f{i}.py")),
                format!("def filler_{i}():\n    pass\n"),
            )
            .unwrap();
        }
        build(
            &root,
            &dir,
            &BuildOpts {
                reader_threads: 1,
                quiet: true,
                phase1_only: false,
                ..Default::default()
            },
        )
        .unwrap();
        let idx = Index::open(&dir).unwrap();
        assert!(idx.manifest.phase2);
        let (a0, b0, c0) = (
            id_of(&idx, "pkg/a.py"),
            id_of(&idx, "pkg/b.py"),
            id_of(&idx, "pkg/c.py"),
        );
        assert_eq!(idx.out_edges(a0).as_ref(), &[b0]);
        assert_eq!(idx.in_edges(b0).as_ref(), &[a0]);
        assert_eq!(idx.in_edges(a0).as_ref(), &[c0]);
        let rank_b = idx.rec(b0).unwrap().rank;
        assert!(rank_b > 0, "phase 2 ranks every file");

        // 1. edit the imported file: same rank, importers still reach it
        std::thread::sleep(Duration::from_millis(20));
        fs::write(
            root.join("pkg/b.py"),
            "def helper():\n    pass\n\ndef helper_two():\n    pass\n",
        )
        .unwrap();
        let idx = edit_and_apply(&idx, &root, &dir);
        let b1 = id_of(&idx, "pkg/b.py");
        assert_ne!(b1, b0);
        assert_eq!(idx.canon(b1), b0);
        assert_eq!(idx.latest(b0), b1);
        assert_eq!(idx.rec(b1).unwrap().rank, rank_b, "rank carried over");
        assert_eq!(
            idx.out_edges(a0).as_ref(),
            &[b1],
            "base edge follows the edit"
        );
        assert_eq!(idx.in_edges(b1).as_ref(), &[a0]);
        assert_eq!(idx.out_edges(b1).len(), 0);
        assert_eq!(idx.lookup("helper_two").len(), 1, "delta symbols");

        // 2. edit the importer to import elsewhere: its delta edges replace the base ones
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.join("pkg/a.py"), "from pkg.c import use\n").unwrap();
        let idx = edit_and_apply(&idx, &root, &dir);
        let a1 = id_of(&idx, "pkg/a.py");
        assert_eq!(
            idx.out_edges(a1).as_ref(),
            &[c0],
            "delta imports resolve against the base"
        );
        assert_eq!(idx.in_edges(b1).len(), 0, "the stale base edge is gone");
        let mut in_c: Vec<u32> = idx.in_edges(c0).to_vec();
        in_c.sort_unstable();
        assert_eq!(in_c, vec![a1]);
        // c still imports a, now at its new id
        assert_eq!(idx.out_edges(c0).as_ref(), &[a1]);
        assert_eq!(idx.in_edges(a1).as_ref(), &[c0]);

        // 3. a new file importing the twice-edited target: neutral rank, resolved edge
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.join("pkg/d.py"), "from pkg.b import helper_two\n").unwrap();
        let idx = edit_and_apply(&idx, &root, &dir);
        let d = id_of(&idx, "pkg/d.py");
        assert_eq!(idx.canon(d), d);
        assert_eq!(idx.rec(d).unwrap().rank, 0);
        assert_eq!(idx.out_edges(d).as_ref(), &[b1]);
        assert_eq!(idx.in_edges(b1).as_ref(), &[d]);
        assert_eq!(idx.manifest.deltas, 3);
        assert!(idx.deltas.iter().all(|s| s.n_files == 1));
        let _ = NONE;
        let _ = fs::remove_dir_all(&base);
    }
}
