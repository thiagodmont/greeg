//! Phase-1 build (ARCHITECTURE.md): walk, read with a small reader pool,
//! extract trigrams and words, merge per-thread maps, write roaring postings,
//! publish.

use crate::Index;
use crate::external;
use crate::format::{self, DirRec, FileRec, FileTable, NONE, is_ignore_file};
use crate::gram::{Dedup, fold_buf};
use crate::resolve::Resolver;
use crate::skipped::Skipped;
use crate::symtab::{DeltaGraphBuilder, FileExtract, GraphBuilder, SpanBuilder, SymBuilder};
use crate::words;
use crate::{Manifest, now_ms, read_manifest, write_manifest};
use anyhow::{Context, Result, bail};
use greeg_lang::sym;
use greeg_lang::{FileFlags, Lang, content_flags, path_flags};
use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

pub const MAX_FILE: u64 = 4 << 20;

#[derive(Clone, Debug)]
pub struct BuildOpts {
    pub reader_threads: usize,
    pub quiet: bool,
    /// Stop after phase 1 (grams only).
    pub phase1_only: bool,
    /// Bytes of postings held across all workers before they spill to sorted
    /// segments (`external.rs`). Below it the build is the in-memory one.
    pub posting_budget: usize,
}

impl Default for BuildOpts {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        BuildOpts {
            reader_threads: if cfg!(target_os = "macos") {
                cores.min(4)
            } else {
                cores
            },
            quiet: true,
            phase1_only: false,
            posting_budget: std::env::var("GREEG_BUILD_BUDGET_MB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map(|mb| mb << 20)
                .unwrap_or(external::DEFAULT_BUDGET),
        }
    }
}

/// What a stat says about a file: its size, modification and change times,
/// inode and device. An edit that restores the size and mtime still moves the
/// ctime, and an atomic replace brings a new inode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub dev: u64,
}

impl Stamp {
    pub fn of(md: &fs::Metadata) -> Stamp {
        if fixed_identity() {
            return Stamp {
                size: md.len(),
                mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
                ..Default::default()
            };
        }
        Stamp {
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
            ino: md.ino(),
            dev: md.dev(),
        }
    }

    /// Equal, ignoring the inode and device when `no_ino`.
    pub fn same(&self, o: &Stamp, no_ino: bool) -> bool {
        self.size == o.size
            && self.mtime_ns == o.mtime_ns
            && self.ctime_ns == o.ctime_ns
            && (no_ino || (self.ino == o.ino && self.dev == o.dev))
    }

    /// A directory's entry list is unchanged: same mtime and (unless
    /// `no_ino`) the same directory.
    pub fn same_dir(&self, o: &Stamp, no_ino: bool) -> bool {
        self.mtime_ns == o.mtime_ns && (no_ino || (self.ino == o.ino && self.dev == o.dev))
    }

    /// Differs only in inode or device.
    pub fn moved_only(&self, o: &Stamp) -> bool {
        self.same(o, true) && !self.same(o, false)
    }

    /// From when a write can no longer keep these timestamps: 20 ms after the
    /// later of mtime and ctime for sub-second timestamps (coarse kernel
    /// clocks), 2 s when the file system records whole seconds.
    pub fn trusted_from(&self) -> i64 {
        let whole = self.mtime_ns % 1_000_000_000 == 0 && self.ctime_ns % 1_000_000_000 == 0;
        let window = if whole { 2_000_000_000 } else { 20_000_000 };
        self.mtime_ns.max(self.ctime_ns) + window
    }

    /// Read at `read_ns`, too soon for a later write to show in the stamp.
    pub fn racy(&self, read_ns: i64) -> bool {
        read_ns < self.trusted_from()
    }
}

/// `GREEG_DEBUG_FIXED_STAMPS=1` records no change time, inode or device, so
/// the layout fingerprint test gets the same bytes on every machine.
fn fixed_identity() -> bool {
    static FIXED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FIXED.get_or_init(|| std::env::var_os("GREEG_DEBUG_FIXED_STAMPS").is_some())
}

pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct WalkedFile {
    pub rel: Vec<u8>,
    pub stamp: Stamp,
    pub dir: u32,
}

#[derive(Clone, Debug)]
pub struct WalkedDir {
    pub rel: Vec<u8>,
    pub stamp: Stamp,
}

/// A walker with scan-mode semantics (hidden skipped, ignore rules honoured)
/// that additionally yields ignore files (`.gitignore`, `.ignore`,
/// `.rgignore`) so the freshness check can see them change. They are stored
/// as tracked-only (`FileTable::hidden`) and never searched.
pub fn walker(path: &Path) -> ignore::WalkBuilder {
    walker_noting(path, None)
}

/// Paths a walker saw and left out for their hidden name.
pub(crate) type Noted = std::sync::Arc<Mutex<Vec<std::path::PathBuf>>>;

/// `walker` that also records the hidden entries it leaves out in `noted`.
/// The filter only sees entries no ignore rule excluded first.
pub(crate) fn walker_noting(path: &Path, noted: Option<Noted>) -> ignore::WalkBuilder {
    let mut wb = ignore::WalkBuilder::new(path);
    wb.hidden(false).filter_entry(move |e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().as_bytes();
        let keep = !name.starts_with(b".")
            || (e.file_type().map(|t| t.is_file()).unwrap_or(false) && is_ignore_file(name));
        if !keep && let Some(n) = &noted {
            n.lock().unwrap().push(e.path().to_path_buf());
        }
        keep
    });
    wb
}

/// Walk `root` with the same ignore semantics as scan mode; returns files and
/// directories sorted by relative path, with the file's dir index resolved.
pub fn walk(root: &Path) -> Result<(Vec<WalkedFile>, Vec<WalkedDir>)> {
    walk_noting(root, None)
}

/// `walk` that also lists what it skipped inside the directories it walked.
/// The listing runs on its own thread (it reads every directory again), so
/// a build overlaps it with extraction; join the handle before publishing.
pub fn walk_with_skipped(
    root: &Path,
) -> Result<(
    Vec<WalkedFile>,
    Vec<WalkedDir>,
    std::thread::JoinHandle<Skipped>,
)> {
    let noted = Noted::default();
    let (files, dirs) = walk_noting(root, Some(noted.clone()))?;
    let noted: Vec<Vec<u8>> = noted
        .lock()
        .unwrap()
        .iter()
        .map(|p| crate::rel::of(root, p))
        .collect();
    let kept: Vec<Vec<u8>> = files
        .iter()
        .map(|f| f.rel.clone())
        .chain(dirs.iter().map(|d| d.rel.clone()))
        .collect();
    let n_files = files.len();
    let root = root.to_path_buf();
    let listing = std::thread::spawn(move || {
        let hidden: HashSet<&[u8]> = noted.iter().map(Vec::as_slice).collect();
        let dirs: Vec<&[u8]> = kept[n_files..].iter().map(Vec::as_slice).collect();
        let kept: HashSet<&[u8]> = kept.iter().map(Vec::as_slice).collect();
        let mut skipped = Skipped::default();
        skipped.update(&crate::skipped::list(&root, &dirs, &kept, &hidden));
        skipped
    });
    Ok((files, dirs, listing))
}

fn walk_noting(root: &Path, noted: Option<Noted>) -> Result<(Vec<WalkedFile>, Vec<WalkedDir>)> {
    type Files = Vec<(Vec<u8>, Stamp)>;
    let out: Mutex<(Files, Vec<WalkedDir>)> =
        Mutex::new((Vec::with_capacity(4096), Vec::with_capacity(512)));
    struct Local<'a> {
        f: Files,
        d: Vec<WalkedDir>,
        out: &'a Mutex<(Files, Vec<WalkedDir>)>,
    }
    impl Local<'_> {
        fn flush(&mut self) {
            let mut g = self.out.lock().unwrap();
            g.0.append(&mut self.f);
            g.1.append(&mut self.d);
        }
    }
    impl Drop for Local<'_> {
        fn drop(&mut self) {
            self.flush();
        }
    }
    let wb = walker_noting(root, noted).build_parallel();
    wb.run(|| {
        let mut local = Local {
            f: Vec::new(),
            d: Vec::new(),
            out: &out,
        };
        Box::new(move |entry| {
            let Ok(e) = entry else {
                return ignore::WalkState::Continue;
            };
            let rel = crate::rel::of(root, e.path());
            let Ok(md) = e.metadata() else {
                return ignore::WalkState::Continue;
            };
            match e.file_type() {
                Some(t) if t.is_dir() => local.d.push(WalkedDir {
                    rel,
                    stamp: Stamp::of(&md),
                }),
                Some(t) if t.is_file() => local.f.push((rel, Stamp::of(&md))),
                _ => {}
            }
            if local.f.len() + local.d.len() >= 512 {
                local.flush();
            }
            ignore::WalkState::Continue
        })
    });
    let (mut files, mut dirs) = out.into_inner().unwrap();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.sort_by(|a, b| a.rel.cmp(&b.rel));
    let files: Vec<WalkedFile> = {
        let dir_index: HashMap<&[u8], u32> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.rel.as_slice(), i as u32))
            .collect();
        files
            .into_iter()
            .map(|(rel, stamp)| {
                let dir = *dir_index.get(crate::rel::parent(&rel)).unwrap_or(&0);
                WalkedFile { rel, stamp, dir }
            })
            .collect()
    };
    Ok((files, dirs))
}

/// Per-file extraction record.
pub struct Extracted {
    pub flags: FileFlags,
    pub grams: Vec<u32>,
    /// What the read saw; `None` when the file was not read.
    pub read: Option<Capture>,
}

/// The stamp of a file as it was read: taken from the open descriptor before
/// the read, so it describes the bytes indexed.
#[derive(Clone, Copy, Debug)]
pub struct Capture {
    pub stamp: Stamp,
    /// The file changed during the read, or since the walk saw it.
    pub changed: bool,
    /// The stamp was too recent to trust (`Stamp::racy`): the hash of the
    /// bytes read, for a build to verify once the timestamps have moved on.
    pub racy: Option<blake3::Hash>,
}

/// Lets tests act while a file is being read, by its name.
#[cfg(test)]
fn read_hook(path: &Path) {
    if path.ends_with("changes-during-read.txt") {
        fs::write(path, "omega\n").unwrap();
    } else if path.ends_with("slow-read.txt") {
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
}

/// Read `path` (at most `MAX_FILE + 1` bytes) into `buf`, sized from the
/// walk's size so the read needs one allocation and no probing.
fn read_file(path: &Path, walk: &Stamp, buf: &mut Vec<u8>) -> Option<Capture> {
    use std::io::Read;
    buf.clear();
    buf.reserve((walk.size as usize).min(MAX_FILE as usize + 1) + 1);
    let f = crate::open_regular(path).ok()?;
    let started = now_ns();
    let before = Stamp::of(&f.metadata().ok()?);
    (&f).take(MAX_FILE + 1).read_to_end(buf).ok()?;
    #[cfg(test)]
    read_hook(path);
    let after = Stamp::of(&f.metadata().ok()?);
    // the inode is compared only on the one descriptor: a path's stat and a
    // descriptor's can disagree on it where inode numbers are not kept, and
    // a replacement since the walk moves the ctime anyway
    let changed = !before.same(&after, false) || !before.same(walk, true);
    let racy = !changed && before.racy(started);
    Some(Capture {
        stamp: before,
        changed,
        racy: racy.then(|| blake3::hash(buf)),
    })
}

/// The stamp and `STAMP_*` flags to record for a file the walk saw as
/// `walk`. A file that was not read keeps the walk's stamp, checked against
/// `walked_ns`, a time no later than the walk's stat.
pub(crate) fn recorded(walk: &Stamp, read: Option<&Capture>, walked_ns: i64) -> (Stamp, u8) {
    let (stamp, recheck) = match read {
        Some(c) => (c.stamp, c.changed || c.racy.is_some()),
        None => (*walk, walk.racy(walked_ns)),
    };
    (stamp, if recheck { format::STAMP_RECHECK } else { 0 })
}

pub(crate) fn file_rec(
    (path_off, path_len): (u32, u16),
    rel: &[u8],
    dir: u32,
    flags: u16,
    rank: u16,
    (stamp, stamp_flags): (Stamp, u8),
) -> FileRec {
    FileRec {
        path_off,
        path_len,
        lang: Lang::from_path(crate::rel::as_path(rel)).code(),
        stamp_flags,
        dir,
        flags,
        rank,
        size: stamp.size,
        mtime_ns: stamp.mtime_ns,
        ctime_ns: stamp.ctime_ns,
        ino: stamp.ino,
        dev: stamp.dev,
    }
}

pub(crate) fn dir_rec((path_off, path_len): (u32, u16), stamp: &Stamp) -> DirRec {
    DirRec {
        path_off,
        path_len,
        pad: 0,
        mtime_ns: stamp.mtime_ns,
        ino: stamp.ino,
        dev: stamp.dev,
    }
}

/// Settle a racy capture (`Capture::racy`): once its timestamps can no
/// longer be reused, re-read the file and clear the doubt when its stamp and
/// bytes still match what was indexed; otherwise it stays `changed`, for the
/// next check to re-extract. Waits at most `max_wait_ns`; a longer wait (a
/// file system with whole-second timestamps, inside a query) leaves the
/// capture racy.
fn settle_racy(path: &Path, c: &mut Capture, max_wait_ns: i64, buf: &mut Vec<u8>) {
    use std::io::Read;
    let Some(hash) = c.racy else { return };
    let wait = c.stamp.trusted_from() - now_ns();
    if wait > max_wait_ns {
        return;
    }
    if wait > 0 {
        std::thread::sleep(std::time::Duration::from_nanos(wait as u64));
    }
    let same = (|| {
        let f = crate::open_regular(path).ok()?;
        let now = Stamp::of(&f.metadata().ok()?);
        buf.clear();
        (&f).take(MAX_FILE + 1).read_to_end(buf).ok()?;
        Some(now.same(&c.stamp, true) && blake3::hash(buf) == hash)
    })()
    .unwrap_or(false);
    c.changed |= !same;
    c.racy = None;
}

/// A build waits out whole-second timestamps too; a query's delta does not.
const BUILD_RACY_WAIT: i64 = 2_000_000_000;
const DELTA_RACY_WAIT: i64 = 25_000_000;

/// Read one file and extract its grams and flags. `buf` and `dd` are reused.
pub fn extract_file(
    path: &Path,
    rel: &[u8],
    walk: &Stamp,
    buf: &mut Vec<u8>,
    dd: &mut Dedup,
    grams: &mut Vec<u32>,
) -> Extracted {
    extract_file_with(path, rel, walk, buf, dd, grams, |_, _| ()).0
}

/// `extract_file`, calling `hook(rel, bytes)` on the unfolded content before
/// gram extraction so a second extractor (symbols) needs no second read.
fn extract_file_with<T: Default>(
    path: &Path,
    rel: &[u8],
    walk: &Stamp,
    buf: &mut Vec<u8>,
    dd: &mut Dedup,
    grams: &mut Vec<u32>,
    hook: impl FnOnce(&[u8], &[u8]) -> T,
) -> (Extracted, T) {
    let mut flags = path_flags(&crate::rel::display(rel));
    grams.clear();
    if walk.size > MAX_FILE {
        flags.set(FileFlags::HUGE);
        return (
            Extracted {
                flags,
                grams: Vec::new(),
                read: None,
            },
            T::default(),
        );
    }
    // ignore files are read only for their stamp: they are never searched
    let read = read_file(path, walk, buf);
    if read.is_none() || is_ignore_file(rel) {
        return (
            Extracted {
                flags,
                grams: Vec::new(),
                read,
            },
            T::default(),
        );
    }
    greeg_lang::transcode_utf16(buf);
    let cf = content_flags(&buf[..buf.len().min(65536)], buf.len() as u64);
    flags.0 |= cf.0;
    if flags.has(FileFlags::BINARY | FileFlags::HUGE) {
        return (
            Extracted {
                flags,
                grams: Vec::new(),
                read,
            },
            T::default(),
        );
    }
    let t = hook(rel, buf);
    fold_buf(buf);
    dd.extract(buf, grams);
    (
        Extracted {
            flags,
            grams: std::mem::take(grams),
            read,
        },
        t,
    )
}

/// Add the words of `src` to `sink` for file `id`, deduplicated per file: the
/// words of one file arrive together, so a word already carrying `id` last was
/// seen in this file. Ids need not be globally monotonic (a worker takes
/// whichever chunk rayon hands it), which is why the dedup is per key and the
/// lists are sorted before they are serialized.
fn push_words(sink: &mut external::Sink<Vec<u8>>, src: &[u8], id: u32) {
    words::for_each_word(src, |w| sink.push_bytes(w, id));
}

/// Add the words of `src` to a plain map for `id`, for the delta path: a delta
/// holds the files of one edit burst, so it is never near the build budget and
/// has no reason to spill.
fn push_words_map(map: &mut HashMap<Vec<u8>, Vec<u32>>, src: &[u8], id: u32) {
    words::for_each_word(src, |w| match map.get_mut(w) {
        Some(v) => {
            if v.last() != Some(&id) {
                v.push(id);
            }
        }
        None => {
            map.insert(w.to_vec(), vec![id]);
        }
    });
}

/// Sorted (word, document count, serialized bitmap) entries of a word map.
fn word_entries(map: HashMap<Vec<u8>, Vec<u32>>) -> Vec<(Vec<u8>, u32, Vec<u8>)> {
    let mut entries: Vec<(Vec<u8>, Vec<u32>)> = map.into_iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(w, v)| {
            let (c, b) = posting_bytes(v);
            (w, c, b)
        })
        .collect()
}

/// Serialize one posting list the way both components want it: a roaring
/// bitmap over the file ids, with the document count beside it.
fn posting_bytes(mut v: Vec<u32>) -> (u32, Vec<u8>) {
    v.sort_unstable();
    v.dedup();
    let bm = RoaringBitmap::from_sorted_iter(v.iter().copied()).unwrap();
    let mut out = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut out).unwrap();
    (v.len() as u32, out)
}

/// How many entries the merge hands to rayon at a time. Large enough that the
/// parallel serialization has work, small enough that the batch is a rounding
/// error against the budget.
const MERGE_BATCH: usize = 4096;

/// Sorted (key, document count, serialized bitmap) entries, from whichever
/// shape the workers ended in. `Memory` is the path that existed before the
/// budget: merge the worker maps, sort, serialize in parallel. `Segments` is
/// the streaming k-way merge, which never holds more than one batch.
fn entries_of<K, T, F>(p: external::Postings<K>, key_out: F) -> Result<Vec<(T, u32, Vec<u8>)>>
where
    K: external::Key,
    T: Send,
    F: Fn(K) -> T + Send + Sync + Copy,
{
    match p {
        external::Postings::Memory(mut maps) => {
            maps.sort_by_key(|m| std::cmp::Reverse(m.len()));
            let mut merged = maps.pop().unwrap_or_default();
            for m in maps {
                for (k, mut v) in m {
                    merged.entry(k).or_default().append(&mut v);
                }
            }
            let mut entries: Vec<(K, Vec<u32>)> = merged.into_iter().collect();
            entries.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
            Ok(entries
                .into_par_iter()
                .map(|(k, v)| {
                    let (c, b) = posting_bytes(v);
                    (key_out(k), c, b)
                })
                .collect())
        }
        external::Postings::Segments(segs) => {
            let mut out: Vec<(T, u32, Vec<u8>)> = Vec::new();
            external::stream_batches::<K>(&segs, MERGE_BATCH, |batch| {
                let mut done: Vec<(T, u32, Vec<u8>)> = batch
                    .into_par_iter()
                    .map(|(k, v)| {
                        let (c, b) = posting_bytes(v);
                        (key_out(k), c, b)
                    })
                    .collect();
                out.append(&mut done);
                Ok(())
            })?;
            for s in &segs {
                let _ = fs::remove_file(s);
            }
            Ok(out)
        }
    }
}

/// Build phase 1 into `dir`. Returns the manifest written.
pub fn build(root: &Path, dir: &Path, opts: &BuildOpts) -> Result<Manifest> {
    let t0 = Instant::now();
    crate::create_private_dir(dir)?;
    crate::write_owner(dir)?;
    // before the walk: a root replaced during the build must not match it
    let root_id = crate::RootId::of(root).context("root identity")?;
    let fsevents_id = greeg_fsevents_id();
    // before the walk, so a change made during it shows up at the next check
    let ignore_inputs = crate::ignores::digest(root);
    let walked_ns = now_ns();
    let (walked, dirs, skipped) = walk_with_skipped(root)?;
    let walk_ms = t0.elapsed().as_secs_f64() * 1e3;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.reader_threads)
        .build()?;
    let root_buf = root.to_path_buf();
    let source_bytes = std::sync::atomic::AtomicU64::new(0);
    type Flags = Vec<(u32, u16, Option<Capture>)>;
    // Postings accumulate against a byte budget shared across the workers and
    // spill to sorted segments when it is reached (`external.rs`, which
    // carries the measurements). A tree that never reaches it never spills
    // and takes the in-memory path unchanged.
    let scratch = external::Scratch::new(dir)?;
    // one accumulator per chunk, and every chunk's accumulator stays alive
    // until the merge, so the budget divides by the chunk count and not by the
    // thread count: the ceiling is what is held at once, not what is running.
    let n_chunks = (opts.reader_threads * 4).max(1);
    let per_chunk = (opts.posting_budget / n_chunks).max(64 << 10);
    type Parts = (
        Flags,
        Vec<external::Part<u32>>,
        Vec<external::Part<Vec<u8>>>,
    );
    let (flags_by_id, gram_parts, word_parts): Parts = pool.install(|| {
        // one accumulator per chunk (a few per thread), not per rayon split:
        // each carries a 2 MiB dedup bitset and a 32k-entry map
        let chunk = walked.len().div_ceil(n_chunks).max(1);
        walked
            .par_chunks(chunk)
            .enumerate()
            .map(|(ci, ws)| -> Result<_> {
                let mut fl: Flags = Vec::with_capacity(ws.len());
                let mut sink: external::Sink<u32> =
                    external::Sink::new(&scratch.dir, "g", ci, per_chunk, 1 << 15);
                let mut wsink: external::Sink<Vec<u8>> =
                    external::Sink::new(&scratch.dir, "w", ci, per_chunk, 1 << 14);
                let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
                let mut dd = Dedup::new();
                let mut grams: Vec<u32> = Vec::with_capacity(8192);
                for (j, w) in ws.iter().enumerate() {
                    let id = (ci * chunk + j) as u32;
                    // words come from the unfolded bytes (case-sensitive keys),
                    // deduplicated per file by the last id pushed
                    let (ex, ()) = extract_file_with(
                        &root_buf.join(crate::rel::as_path(&w.rel)),
                        &w.rel,
                        &w.stamp,
                        &mut buf,
                        &mut dd,
                        &mut grams,
                        |_, src| push_words(&mut wsink, src, id),
                    );
                    if !ex.grams.is_empty() {
                        source_bytes
                            .fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    for &g in &ex.grams {
                        sink.push(&g, id);
                    }
                    grams = ex.grams;
                    fl.push((id, ex.flags.0, ex.read));
                    // only ever between files: a file's postings must not be
                    // split across a spill, or the per-file dedup breaks
                    sink.spill_if_full()?;
                    wsink.spill_if_full()?;
                }
                Ok((fl, vec![sink.finish()?], vec![wsink.finish()?]))
            })
            .try_reduce(
                || (Vec::new(), Vec::new(), Vec::new()),
                |(mut fa, mut ma, mut wa), (mut fb, mut mb, mut wb)| {
                    fa.append(&mut fb);
                    ma.append(&mut mb);
                    wa.append(&mut wb);
                    Ok((fa, ma, wa))
                },
            )
    })?;
    let extract_ms = t0.elapsed().as_secs_f64() * 1e3 - walk_ms;

    // postings: merged, serialized and released one component at a time, so
    // the peak carries one component's entries and one component's bytes
    // rather than both components' of each, and nothing survives into phase 2
    let gram_postings = external::collect(gram_parts, &scratch.dir, "g")?;
    let word_postings = external::collect(word_parts, &scratch.dir, "w")?;
    let spilled = matches!(gram_postings, external::Postings::Segments(_));
    let (n_grams, grams_bytes) = {
        let entries: Vec<(u32, u32, Vec<u8>)> = entries_of(gram_postings, |k| k)?;
        (entries.len(), format::serialize_grams(&entries))
    };
    let (n_words, words_bytes) = {
        let wentries: Vec<(Vec<u8>, u32, Vec<u8>)> = entries_of(word_postings, |k| k)?;
        (wentries.len(), words::serialize(&wentries))
    };
    drop(scratch);

    // file table
    let mut ft = FileTable::default();
    let mut flags_sorted = flags_by_id;
    flags_sorted.sort_unstable_by_key(|(id, _, _)| *id);
    let mut again = Vec::new();
    for (id, _, c) in &mut flags_sorted {
        if let Some(c) = c {
            let path = root.join(crate::rel::as_path(&walked[*id as usize].rel));
            settle_racy(&path, c, BUILD_RACY_WAIT, &mut again);
        }
    }
    for (i, w) in walked.iter().enumerate() {
        let at = ft.intern(&w.rel);
        let (flags, read) = flags_sorted
            .get(i)
            .map(|(_, f, r)| (*f, r.as_ref()))
            .unwrap_or((0, None));
        let rec = file_rec(
            at,
            &w.rel,
            w.dir,
            flags,
            0,
            recorded(&w.stamp, read, walked_ns),
        );
        ft.push_file(&w.rel, rec);
    }
    for d in &dirs {
        let at = ft.intern(&d.rel);
        ft.dirs.push(dir_rec(at, &d.stamp));
    }

    // publish under the writer lock: components, then the manifest, and only
    // then the previous generation and its deltas (readers that opened the old
    // manifest keep valid maps; readers that open the new one ignore delta/)
    let lock = crate::lock::writer(dir)?;
    let previous = read_manifest(dir);
    let generation = previous.as_ref().map(|m| m.generation + 1).unwrap_or(1);
    format::write_atomic(
        &dir.join(format!("grams.{generation}.bin")),
        format::COMP_GRAMS,
        &grams_bytes,
    )?;
    drop(grams_bytes);
    format::write_atomic(
        &dir.join(format!("words.{generation}.bin")),
        format::COMP_WORDS,
        &words_bytes,
    )?;
    drop(words_bytes);
    format::write_atomic(
        &dir.join(format!("files.{generation}.bin")),
        format::COMP_FILES,
        &ft.serialize(),
    )?;
    let skipped = skipped
        .join()
        .map_err(|_| anyhow::anyhow!("listing skipped entries panicked"))?;
    let skipped_name = format!("skipped.{generation}.bin");
    skipped.write(&dir.join(&skipped_name))?;
    let m = Manifest {
        format: crate::FORMAT_VERSION,
        root: root.to_string_lossy().into_owned(),
        root_id,
        generation,
        phase1: true,
        phase2: false,
        symbols: 0,
        edges: 0,
        parse_fallbacks: 0,
        phase2_ms: 0.0,
        files: walked.len() as u32,
        dirs: dirs.len() as u32,
        source_bytes: source_bytes.load(std::sync::atomic::Ordering::Relaxed),
        built_unix_ms: now_ms(),
        build_ms: t0.elapsed().as_secs_f64() * 1e3,
        peak_rss: crate::peak_rss_bytes().unwrap_or(0),
        spilled,
        fsevents_id,
        verified_unix_ms: now_ms(),
        deltas: 0,
        tombstones: 0,
        ignore_inputs,
        skipped: skipped_name,
        // the file system does not change with a rebuild
        stamp_mode: previous.map(|m| m.stamp_mode).unwrap_or_default(),
    };
    write_manifest(dir, &m)?;
    // a fresh build supersedes deltas and older generations
    let _ = fs::remove_dir_all(dir.join("delta"));
    remove_stale(dir, &["grams.", "words.", "files.", "skipped."], generation);
    drop(lock);
    if !opts.quiet {
        eprintln!(
            "greeg index: {} files, {} dirs, {:.1} MB source; walk {:.0} ms, extract {:.0} ms, total {:.0} ms; {} grams, {} words; {} postings, peak RSS {}",
            walked.len(),
            dirs.len(),
            m.source_bytes as f64 / 1e6,
            walk_ms,
            extract_ms,
            m.build_ms,
            n_grams,
            n_words,
            if spilled { "spilled" } else { "in-memory" },
            crate::fmt_bytes(m.peak_rss)
        );
    }
    if opts.phase1_only {
        return Ok(m);
    }
    let mut m = m;
    phase2(root, dir, &mut ft, &walked, &mut m, opts)?;
    Ok(m)
}

/// Quantize a normalized rank (0..=1) into the file table.
pub fn quantize_rank(r: f32) -> u16 {
    1 + (r.clamp(0.0, 1.0) * 65534.0) as u16
}

/// Should this file go through stage B?
pub fn wants_symbols(rel: &[u8], flags: u16) -> bool {
    Lang::from_path(crate::rel::as_path(rel)).has_grammar()
        && !FileFlags(flags)
            .has(FileFlags::BINARY | FileFlags::HUGE | FileFlags::MINIFIED | FileFlags::LOCKFILE)
}

/// Read + extract one file for stage B. The flag says the file no longer
/// matches `recorded`, the stamp its phase-1 record holds, or was read too
/// soon for its stamp to tell.
pub fn extract_symbols(
    path: &Path,
    rel: &[u8],
    recorded: &Stamp,
    buf: &mut Vec<u8>,
) -> (Option<FileExtract>, bool) {
    let Some(read) = read_file(path, recorded, buf) else {
        return (None, true);
    };
    let recheck = read.changed || read.racy.is_some();
    greeg_lang::transcode_utf16(buf);
    if buf.len() as u64 > MAX_FILE || buf[..buf.len().min(8192)].contains(&0) {
        return (None, recheck);
    }
    (Some(symbols_of_bytes(rel, buf)), recheck)
}

fn symbols_of_bytes(rel: &[u8], src: &[u8]) -> FileExtract {
    let lang = Lang::from_path(crate::rel::as_path(rel));
    let ex = sym::extract(lang, sym::is_tsx(&crate::rel::display(rel)), src);
    FileExtract::from_extract(ex, src)
}

/// Phase 2 (ARCHITECTURE.md): parse every file with a grammar
/// on all cores, resolve imports, run PageRank, publish symbols/spans/graph
/// and republish the file table with ranks and parse flags.
fn phase2(
    root: &Path,
    dir: &Path,
    ft: &mut FileTable,
    walked: &[WalkedFile],
    m: &mut Manifest,
    opts: &BuildOpts,
) -> Result<()> {
    let t0 = Instant::now();
    let generation = m.generation;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cores).build()?;
    let root_buf = root.to_path_buf();
    let todo: Vec<u32> = (0..ft.files.len())
        .filter(|&i| wants_symbols(&walked[i].rel, ft.files[i].flags))
        .map(|i| i as u32)
        .collect();
    let extracts: Vec<(u32, (Option<FileExtract>, bool))> = pool.install(|| {
        sym::warm();
        todo.par_iter()
            .map_init(
                || Vec::<u8>::with_capacity(256 * 1024),
                |buf, &i| {
                    let w = &walked[i as usize];
                    let rec = ft.files[i as usize].stamp();
                    (
                        i,
                        extract_symbols(
                            &root_buf.join(crate::rel::as_path(&w.rel)),
                            &w.rel,
                            &rec,
                            buf,
                        ),
                    )
                },
            )
            .collect()
    });
    let parse_ms = t0.elapsed().as_secs_f64() * 1e3;
    let mut by_file: Vec<Option<FileExtract>> = (0..ft.files.len()).map(|_| None).collect();
    let mut fallbacks = 0u32;
    for (i, (ex, changed)) in extracts {
        // parsed from other bytes than the phase-1 record describes
        if changed {
            ft.files[i as usize].stamp_flags |= format::STAMP_RECHECK;
        }
        if let Some(ex) = &ex {
            if !ex.tree_sitter {
                fallbacks += 1;
            }
            if ex.parse_errors || !ex.tree_sitter {
                ft.files[i as usize].flags |= FileFlags::PARSE_ERRORS;
            }
        }
        by_file[i as usize] = ex;
    }
    // resolve imports → graph; a path that is not UTF-8 is never named by
    // an import, so it neither resolves nor is resolved
    let rels: Vec<(u32, &str)> = walked
        .iter()
        .enumerate()
        .filter_map(|(i, w)| Some((i as u32, std::str::from_utf8(&w.rel).ok()?)))
        .collect();
    let kt = by_file.iter().enumerate().filter_map(|(i, ex)| {
        let ex = ex.as_ref()?;
        let pkg = ex.package.as_deref()?;
        Some((i as u32, pkg, ex.top_level_names().collect::<Vec<_>>()))
    });
    let resolver = Resolver::new(root, &rels, kt);
    drop(rels);
    let n = ft.files.len() as u32;
    let mut graph = GraphBuilder::new(n);
    let mut targets: Vec<Vec<u32>> = vec![Vec::new(); ft.files.len()];
    for (i, ex) in by_file.iter().enumerate() {
        let Some(ex) = ex else { continue };
        let Ok(rel) = std::str::from_utf8(&walked[i].rel) else {
            targets[i] = vec![NONE; ex.imports.len()];
            continue;
        };
        let lang = Lang::from_path(Path::new(rel));
        let ctx = resolver.file_ctx(lang, rel);
        let mut t = Vec::with_capacity(ex.imports.len());
        for im in &ex.imports {
            let ids = resolver.resolve_with(lang, rel, &ctx, im);
            t.push(ids.first().copied().unwrap_or(NONE));
            let w = (im.names.len().max(1) * 10).min(u16::MAX as usize) as u16;
            for id in ids {
                graph.add(i as u32, id, w);
            }
        }
        if lang == Lang::Kotlin
            && let Some(pkg) = &ex.package
        {
            for id in resolver.kotlin_package_peers(pkg, i as u32) {
                graph.add(i as u32, id, 1);
            }
        }
        targets[i] = t;
    }
    let n_edges = graph.n_edges();
    let (graph_body, rank) = graph.finish();
    for (i, f) in ft.files.iter_mut().enumerate() {
        f.rank = quantize_rank(rank[i]);
    }
    let resolve_ms = t0.elapsed().as_secs_f64() * 1e3 - parse_ms;
    drop(resolver);
    // symbols + spans
    let mut sb = SymBuilder::new(n);
    let mut pb = SpanBuilder::new(n);
    for i in 0..ft.files.len() {
        // both builders copy what they need out of the extract, so release it
        // here rather than holding every file's parse until the two bodies are
        // serialized: on rust-lang/rust that is 428k symbols kept alongside
        // the 40 MB they serialize into
        let ex = by_file[i].take();
        let t = std::mem::take(&mut targets[i]);
        sb.add_file(i as u32, ex.as_ref());
        pb.add_file(i as u32, ex.as_ref(), &t);
    }
    drop(targets);
    let n_symbols = sb.n_symbols() as u32;
    let sym_body = sb.finish(&|f| rank[f as usize]);
    let span_body = pb.finish();
    drop(by_file);
    // publish under the lock; queries may have applied deltas against this
    // generation meanwhile, so the manifest keeps their count
    let lock = crate::lock::writer(dir)?;
    let Some(cur) = read_manifest(dir) else {
        bail!("manifest vanished during phase 2")
    };
    if cur.generation != generation {
        bail!(
            "index generation {} superseded by {} during phase 2",
            generation,
            cur.generation
        );
    }
    format::write_atomic(
        &dir.join(format!("symbols.{generation}.bin")),
        format::COMP_SYMBOLS,
        &sym_body,
    )?;
    format::write_atomic(
        &dir.join(format!("spans.{generation}.bin")),
        format::COMP_SPANS,
        &span_body,
    )?;
    format::write_atomic(
        &dir.join(format!("graph.{generation}.bin")),
        format::COMP_GRAPH,
        &graph_body,
    )?;
    format::write_atomic(
        &dir.join(format!("files.{generation}.bin")),
        format::COMP_FILES,
        &ft.serialize(),
    )?;
    m.deltas = cur.deltas;
    m.tombstones = cur.tombstones;
    m.skipped = cur.skipped;
    m.verified_unix_ms = cur.verified_unix_ms;
    m.fsevents_id = cur.fsevents_id;
    m.phase2 = true;
    m.symbols = n_symbols;
    m.edges = n_edges as u32;
    m.parse_fallbacks = fallbacks;
    m.phase2_ms = t0.elapsed().as_secs_f64() * 1e3;
    m.build_ms += m.phase2_ms;
    // the high-water mark spans both phases, so this only ever grows
    m.peak_rss = m.peak_rss.max(crate::peak_rss_bytes().unwrap_or(0));
    write_manifest(dir, m)?;
    remove_stale(dir, &["symbols.", "spans.", "graph."], generation);
    drop(lock);
    if !opts.quiet {
        eprintln!(
            "greeg index phase 2: {} files parsed on {} threads in {:.0} ms ({} regex fallbacks), resolve+rank {:.0} ms, {} symbols, {} edges, total {:.0} ms",
            todo.len(),
            cores,
            parse_ms,
            fallbacks,
            resolve_ms,
            n_symbols,
            n_edges,
            m.phase2_ms
        );
        eprintln!(
            "greeg index: done in {:.0} ms, peak RSS {}",
            m.build_ms,
            crate::fmt_bytes(m.peak_rss)
        );
    }
    Ok(())
}

/// Delete component files of other generations (and leftover temp files).
fn remove_stale(dir: &Path, prefixes: &[&str], generation: u32) {
    let keep = format!(".{generation}.");
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stale_bin = prefixes.iter().any(|p| name.starts_with(p))
                && name.ends_with(".bin")
                && !name.contains(&keep);
            let stale_tmp = name.ends_with(".tmp")
                && e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs() > 600)
                    .unwrap_or(false);
            if stale_bin || stale_tmp {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

fn greeg_fsevents_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        greeg_fsevents::current_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Build a delta segment for `files` (absolute ids assigned by the caller,
/// starting at `first_id`). `prev[i]` is the id of the file version that
/// `files[i]` supersedes (`NONE` for a new file): its rank is carried over,
/// and `tomb` holds every id this delta retires. Imports are resolved against
/// the live base plus the delta's own files, so the edited files keep their
/// graph edges (ARCHITECTURE.md). Returns the serialized segment body.
pub fn build_delta(
    idx: &Index,
    root: &Path,
    first_id: u32,
    files: &[WalkedFile],
    prev: &[u32],
    dirs: &[WalkedDir],
    tomb: &RoaringBitmap,
) -> Result<Vec<u8>> {
    // unread files (above the size cap) keep the check's stamp, judged
    // against this time
    let started_ns = now_ns();
    let mut buf = Vec::with_capacity(256 * 1024);
    let mut again = Vec::new();
    let mut dd = Dedup::new();
    let mut grams = Vec::with_capacity(8192);
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut wmap: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    let mut ft = FileTable::default();
    let n = files.len() as u32;
    let mut sb = SymBuilder::new(n);
    let mut pb = SpanBuilder::new(n);
    let mut dg = DeltaGraphBuilder::new(n);
    let mut extracts: Vec<Option<FileExtract>> = Vec::with_capacity(files.len());
    let mut ranks: Vec<u16> = Vec::with_capacity(files.len());
    for (i, w) in files.iter().enumerate() {
        let id = first_id + i as u32;
        // one read serves both extractors: symbols see the unfolded bytes, then grams
        let (ex, fx) = extract_file_with(
            &root.join(crate::rel::as_path(&w.rel)),
            &w.rel,
            &w.stamp,
            &mut buf,
            &mut dd,
            &mut grams,
            |rel, src| {
                push_words_map(&mut wmap, src, id);
                let flags = path_flags(&crate::rel::display(rel)).0
                    | content_flags(&src[..src.len().min(65536)], src.len() as u64).0;
                if wants_symbols(rel, flags) {
                    Some(symbols_of_bytes(rel, src))
                } else {
                    None
                }
            },
        );
        for &g in &ex.grams {
            map.entry(g).or_default().push(id);
        }
        grams = ex.grams;
        let mut flags = ex.flags.0;
        if let Some(fx) = &fx
            && (fx.parse_errors || !fx.tree_sitter)
        {
            flags |= FileFlags::PARSE_ERRORS;
        }
        let prev_id = prev.get(i).copied().unwrap_or(NONE);
        dg.set_prev(i as u32, prev_id);
        // a modified file keeps the rank of the version it replaces; new files are neutral
        let rank = if prev_id == NONE {
            0
        } else {
            idx.rec(prev_id).map(|r| r.rank).unwrap_or(0)
        };
        ranks.push(rank);
        sb.add_file(i as u32, fx.as_ref());
        extracts.push(fx);
        let mut read = ex.read;
        if let Some(c) = &mut read {
            settle_racy(
                &root.join(crate::rel::as_path(&w.rel)),
                c,
                DELTA_RACY_WAIT,
                &mut again,
            );
        }
        let at = ft.intern(&w.rel);
        let rec = file_rec(
            at,
            &w.rel,
            w.dir,
            flags,
            rank,
            recorded(&w.stamp, read.as_ref(), started_ns),
        );
        ft.push_file(&w.rel, rec);
    }
    // imports: resolve against live base files (minus this delta's tombstones)
    // plus the delta's own files, which shadow the versions they replace
    let needs_resolver = extracts
        .iter()
        .flatten()
        .any(|fx| !fx.imports.is_empty() || fx.package.is_some());
    let targets: Vec<Vec<u32>> =
        if needs_resolver {
            let mut rels: Vec<(u32, &str)> =
                Vec::with_capacity(idx.base.n_files as usize + files.len());
            rels.extend(
                idx.live_files()
                    .filter(|(id, _, _)| !tomb.contains(*id))
                    .filter_map(|(id, rel, _)| Some((id, std::str::from_utf8(rel).ok()?))),
            );
            rels.extend(files.iter().enumerate().filter_map(|(i, w)| {
                Some((first_id + i as u32, std::str::from_utf8(&w.rel).ok()?))
            }));
            let kt = extracts.iter().enumerate().filter_map(|(i, ex)| {
                let ex = ex.as_ref()?;
                let pkg = ex.package.as_deref()?;
                Some((
                    first_id + i as u32,
                    pkg,
                    ex.top_level_names().collect::<Vec<_>>(),
                ))
            });
            let resolver = Resolver::new(root, &rels, kt);
            let kt_base = KotlinBase::new(idx, tomb, &extracts);
            extracts
                .iter()
                .enumerate()
                .map(|(i, ex)| {
                    let Some(ex) = ex else {
                        return Vec::new();
                    };
                    let Ok(rel) = std::str::from_utf8(&files[i].rel) else {
                        return vec![NONE; ex.imports.len()];
                    };
                    let lang = Lang::from_path(Path::new(rel));
                    let ctx = resolver.file_ctx(lang, rel);
                    let me = first_id + i as u32;
                    let mut t = Vec::with_capacity(ex.imports.len());
                    for im in &ex.imports {
                        let mut ids = resolver.resolve_with(lang, rel, &ctx, im);
                        if ids.is_empty() && lang == Lang::Kotlin {
                            ids = kt_base.resolve(&im.module, im.wildcard, me);
                        }
                        t.push(ids.first().copied().unwrap_or(NONE));
                        let w = (im.names.len().max(1) * 10).min(u16::MAX as usize) as u16;
                        for id in ids {
                            dg.add(i as u32, id, w);
                        }
                    }
                    if lang == Lang::Kotlin
                        && let Some(pkg) = &ex.package
                    {
                        for id in resolver.kotlin_package_peers(pkg, me) {
                            dg.add(i as u32, id, 1);
                        }
                        for id in kt_base.peers(pkg, me) {
                            dg.add(i as u32, id, 1);
                        }
                    }
                    t
                })
                .collect()
        } else {
            extracts
                .iter()
                .map(|ex| vec![NONE; ex.as_ref().map(|f| f.imports.len()).unwrap_or(0)])
                .collect()
        };
    for (i, ex) in extracts.iter().enumerate() {
        pb.add_file(i as u32, ex.as_ref(), &targets[i]);
    }
    for d in dirs {
        let at = ft.intern(&d.rel);
        ft.dirs.push(dir_rec(at, &d.stamp));
    }
    let mut entries: Vec<(u32, u32, Vec<u8>)> = map
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            let bm = RoaringBitmap::from_sorted_iter(v.iter().copied()).unwrap();
            let mut out = Vec::new();
            bm.serialize_into(&mut out).unwrap();
            (k, v.len() as u32, out)
        })
        .collect();
    entries.sort_unstable_by_key(|(k, _, _)| *k);
    let fb = ft.serialize();
    let gb = format::serialize_grams(&entries);
    let wb = words::serialize(&word_entries(wmap));
    let sbb = sb.finish(&|f| {
        ranks
            .get(f as usize)
            .filter(|&&r| r > 0)
            .map(|&r| (r - 1) as f32 / 65534.0)
            .unwrap_or(0.5)
    });
    let pbb = pb.finish();
    let gbb = dg.finish();
    let mut tb = Vec::new();
    tomb.serialize_into(&mut tb)?;
    let mut body = Vec::with_capacity(
        format::DELTA_HEADER
            + fb.len()
            + gb.len()
            + sbb.len()
            + pbb.len()
            + gbb.len()
            + wb.len()
            + tb.len(),
    );
    for x in [
        first_id,
        n,
        fb.len() as u32,
        gb.len() as u32,
        sbb.len() as u32,
        pbb.len() as u32,
        gbb.len() as u32,
        wb.len() as u32,
    ] {
        body.extend_from_slice(&x.to_le_bytes());
    }
    debug_assert_eq!(body.len(), format::DELTA_HEADER);
    body.extend_from_slice(&fb);
    body.extend_from_slice(&gb);
    body.extend_from_slice(&sbb);
    body.extend_from_slice(&pbb);
    body.extend_from_slice(&gbb);
    body.extend_from_slice(&wb);
    body.extend_from_slice(&tb);
    Ok(body)
}

/// Kotlin resolution against the base at delta time. The base does not store
/// packages, so `import a.b.C` is matched by a top-level symbol `C` in a live
/// Kotlin file whose directory ends with `a/b` (the source-root convention),
/// and `a.b.*` or same-package peers by the directory suffix alone.
struct KotlinBase<'a> {
    idx: &'a Index,
    tomb: &'a RoaringBitmap,
    /// Live base Kotlin files as (id, rel); empty when the delta has no Kotlin file.
    files: Vec<(u32, &'a str)>,
}

impl<'a> KotlinBase<'a> {
    fn new(idx: &'a Index, tomb: &'a RoaringBitmap, extracts: &[Option<FileExtract>]) -> Self {
        let wanted = extracts.iter().flatten().any(|fx| fx.package.is_some());
        let files = if wanted {
            idx.live_files()
                .filter(|(id, rel, _)| !tomb.contains(*id) && rel.ends_with(b".kt"))
                .filter_map(|(id, rel, _)| Some((id, std::str::from_utf8(rel).ok()?)))
                .collect()
        } else {
            Vec::new()
        };
        KotlinBase { idx, tomb, files }
    }
    fn dir_matches(rel: &str, pkg_path: &str) -> bool {
        let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        dir == pkg_path
            || (dir.len() > pkg_path.len()
                && dir.ends_with(pkg_path)
                && dir.as_bytes()[dir.len() - pkg_path.len() - 1] == b'/')
    }
    fn in_package(&self, pkg: &str, exclude: u32) -> Vec<u32> {
        let pkg_path = pkg.replace('.', "/");
        self.files
            .iter()
            .filter(|(id, rel)| *id != exclude && Self::dir_matches(rel, &pkg_path))
            .map(|(id, _)| *id)
            .collect()
    }
    fn resolve(&self, module: &str, wildcard: bool, me: u32) -> Vec<u32> {
        if self.files.is_empty() {
            return Vec::new();
        }
        if wildcard {
            return self.in_package(module, me);
        }
        // longest package prefix whose next segment is a top-level symbol
        let segs: Vec<&str> = module.split('.').collect();
        for k in (1..segs.len()).rev() {
            let pkg_path = segs[..k].join("/");
            let name = segs[k];
            let mut out: Vec<u32> = self
                .idx
                .lookup(name)
                .into_iter()
                .filter(|s| self.idx.sym_parent(*s).is_none())
                .map(|s| self.idx.sym_file(s))
                .filter(|&id| {
                    id != me
                        && !self.tomb.contains(id)
                        && self
                            .idx
                            .path(id)
                            .and_then(|rel| std::str::from_utf8(rel).ok())
                            .map(|rel| rel.ends_with(".kt") && Self::dir_matches(rel, &pkg_path))
                            .unwrap_or(false)
                })
                .collect();
            if !out.is_empty() {
                out.sort_unstable();
                out.dedup();
                return out;
            }
        }
        Vec::new()
    }
    /// Files sharing package `pkg` by directory suffix, capped as in the base build.
    fn peers(&self, pkg: &str, me: u32) -> Vec<u32> {
        let v = self.in_package(pkg, me);
        if v.len() <= 30 { v } else { Vec::new() }
    }
}

pub fn root_of(p: &Path) -> PathBuf {
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory this test creates itself, never one left by another run.
    fn temp(name: &str) -> std::path::PathBuf {
        for n in 0.. {
            let d =
                std::env::temp_dir().join(format!("greeg-stamp-{name}-{}-{n}", std::process::id()));
            match fs::create_dir(&d) {
                Ok(()) => return d,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create {}: {e}", d.display()),
            }
        }
        unreachable!()
    }

    #[test]
    fn stamps_too_recent_to_trust_depend_on_timestamp_resolution() {
        let fine = Stamp {
            mtime_ns: 5_000_000_123,
            ctime_ns: 5_000_000_456,
            ..Default::default()
        };
        assert!(fine.racy(5_010_000_000));
        assert!(!fine.racy(5_030_000_000));
        let whole = Stamp {
            mtime_ns: 5_000_000_000,
            ctime_ns: 5_000_000_000,
            ..Default::default()
        };
        assert!(whole.racy(6_500_000_000));
        assert!(!whole.racy(7_000_000_001));
    }

    #[test]
    fn a_racy_capture_is_cleared_only_when_its_bytes_still_match() {
        let d = temp("racy");
        let p = d.join("a.txt");
        let mut buf = Vec::new();
        for (edit, changed) in [(false, false), (true, true)] {
            fs::write(&p, "alpha\n").unwrap();
            let walk = Stamp::of(&fs::symlink_metadata(&p).unwrap());
            let mut c = read_file(&p, &walk, &mut buf).unwrap();
            assert!(c.racy.is_some() && !c.changed, "just written");
            if edit {
                // same size, stamps put back: only the bytes tell
                let mt = fs::metadata(&p).unwrap().modified().unwrap();
                fs::write(&p, "bravo\n").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&p)
                    .unwrap()
                    .set_modified(mt)
                    .unwrap();
            }
            settle_racy(&p, &mut c, BUILD_RACY_WAIT, &mut buf);
            assert!(c.racy.is_none());
            assert_eq!(c.changed, changed, "edited: {edit}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_read_that_started_too_soon_after_a_write_is_racy_however_long_it_takes() {
        let d = temp("slow");
        let f = d.join("slow-read.txt");
        fs::write(&f, "alpha\n").unwrap();
        let walk = Stamp::of(&fs::symlink_metadata(&f).unwrap());
        // the hook sleeps past the trust window while the file is read
        let c = read_file(&f, &walk, &mut Vec::new()).unwrap();
        assert!(!c.changed && c.racy.is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn symbols_read_too_soon_after_a_write_are_rechecked() {
        let d = temp("symbols");
        let f = d.join("a.rs");
        fs::write(&f, "fn alpha() {}\n").unwrap();
        let recorded = Stamp::of(&fs::symlink_metadata(&f).unwrap());
        let (ex, recheck) = extract_symbols(&f, b"a.rs", &recorded, &mut Vec::new());
        assert!(ex.is_some() && recheck);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_walk_stamp_that_differs_only_in_inode_is_not_a_change() {
        let d = temp("walk-ino");
        let f = d.join("a.txt");
        fs::write(&f, "alpha\n").unwrap();
        let walk = Stamp::of(&fs::symlink_metadata(&f).unwrap());
        let mut buf = Vec::new();
        let other_ino = Stamp {
            ino: walk.ino + 1,
            ..walk
        };
        assert!(!read_file(&f, &other_ino, &mut buf).unwrap().changed);
        let other_size = Stamp {
            size: walk.size + 1,
            ..walk
        };
        assert!(read_file(&f, &other_size, &mut buf).unwrap().changed);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_changed_while_it_is_indexed_is_rechecked() {
        let d = temp("during");
        let root = d.join("tree");
        let dir = d.join("index");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("changes-during-read.txt"), "alpha\n").unwrap();
        fs::write(root.join("steady.txt"), "steady\n").unwrap();
        build(
            &root,
            &dir,
            &BuildOpts {
                reader_threads: 1,
                quiet: true,
                phase1_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        let idx = crate::Index::open(&dir).unwrap();
        let marked: Vec<&[u8]> = idx
            .live_files()
            .filter(|(_, _, r)| r.stamp_flags & format::STAMP_RECHECK != 0)
            .map(|(_, rel, _)| rel)
            .collect();
        assert_eq!(marked, [b"changes-during-read.txt"]);
        let ch = crate::fresh::check(&idx, &root, crate::fresh::Mode::Stat, 1).unwrap();
        assert_eq!(
            ch.modified
                .iter()
                .map(|m| m.1.rel.as_slice())
                .collect::<Vec<_>>(),
            [b"changes-during-read.txt"]
        );
        let _ = fs::remove_dir_all(&d);
    }
}
