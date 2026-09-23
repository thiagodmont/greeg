//! Bounded-memory posting accumulation for the phase-1 build.
//!
//! The straightforward build holds one `Vec<u32>` per distinct gram and per
//! distinct word for the whole repository, merges every worker's map into one,
//! and only then serializes. Nothing in that bounds peak memory by anything
//! but the size of the tree: unbounded, rust-lang/rust (62k files, 188 MB of
//! source) peaks at 601–647 MB and TypeScript-5.9 (353 MB) at 503 MB —
//! roughly 3× the source, with the `Vec` slack and the transient second copy
//! of the merged map on top.
//! The build runs detached behind the first query, so that spike is invisible
//! until something else on the machine is killed for it.
//!
//! This module bounds it to a byte budget instead:
//!
//! 1. Each worker accumulates postings in a map with a byte budget.
//! 2. When the budget is reached *between files*, the map is written to a
//!    sorted segment file and its allocation released.
//! 3. [`Merged::stream`] k-way merges the segments back in key order, so the
//!    serializer sees exactly the ascending, deduplicated postings it saw
//!    before.
//!
//! Peak is then `budget + the largest single posting list + the component
//! being serialized`, independent of the number of files.
//!
//! **A repository that never fills the budget never spills**, and
//! [`Postings::Memory`] is byte-for-byte the pre-existing path: one merge of
//! the worker maps, one parallel serialization. Small and mid-size trees pay
//! nothing for this, not even a `stat`.
//!
//! ## Segment format (temporary, never published)
//!
//! Records in ascending key order, to end of file:
//!
//! ```text
//! varint key_len | key bytes | varint n_files | varint file[0] | varint (file[i] - file[i-1])
//! ```
//!
//! File ids within a record ascend, so the deltas are non-negative. The same
//! key may appear in several segments (different workers, or the same worker
//! before and after a spill); the merge concatenates and re-sorts those lists,
//! which is why a worker's ids need not be globally monotonic.

use anyhow::{Context, Result};
use hashbrown::HashMap;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Total bytes of postings held across all workers before spilling.
///
/// Measured on the reference machine, phase 1, against the unbounded build
/// run back to back with this one. Peak is `/usr/bin/time -l`, best of three;
/// CPU is `RUSAGE_CHILDREN` user+sys, best of three — wall clock on a loaded
/// machine reads the opposite way round, because the unbounded build has the
/// larger footprint and is penalised twice under memory pressure.
///
/// | corpus | source | peak, unbounded → budgeted | CPU |
/// |---|---|---|---|
/// | tokio | 6 MB | 101 → 99 MB | 1.08× |
/// | ktor | 16 MB | 135 → 137 MB | 0.85× |
/// | django | 38 MB | 205 → 194 MB | 1.03× |
/// | TypeScript-5.9 | 353 MB | 481 → 338 MB | 1.04× |
/// | rust-lang/rust | 188 MB | 615 → 356 MB | 1.09× |
///
/// Only the last two reach the budget and spill; the first three take the
/// in-memory path and are within noise of it either way. 192 MB is where that
/// split falls: it leaves the trees whose peak was never a problem alone, and
/// charges about 9 % of one background build's CPU on the two that were, to
/// give back 140–260 MB of a machine that is also running the agent.
///
/// Spilling is not free — the k-way merge re-reads what the segments wrote.
/// What it buys is a peak that stops tracking the size of the tree.
pub const DEFAULT_BUDGET: usize = 192 << 20;

/// Per-segment read-ahead during the merge; shared out across the open
/// segments so fan-in does not multiply it.
const MERGE_READ_BUDGET: usize = 4 << 20;
const MIN_SEGMENT_BUF: usize = 16 << 10;

/// A posting key. Implemented for `u32` (grams, stored big-endian so that byte
/// order and numeric order agree) and `Vec<u8>` (words, stored as themselves).
pub trait Key: Clone + Eq + std::hash::Hash + Ord + Send + Sync {
    fn write_key(&self, out: &mut Vec<u8>);
    fn from_key_bytes(b: &[u8]) -> Self;
    /// Bytes this key costs on the heap, key material plus map slot.
    fn key_heap_bytes(&self) -> usize;
}

impl Key for u32 {
    fn write_key(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn from_key_bytes(b: &[u8]) -> Self {
        let mut a = [0u8; 4];
        a.copy_from_slice(b);
        u32::from_be_bytes(a)
    }
    fn key_heap_bytes(&self) -> usize {
        // u32 key + Vec header, in a hashbrown slot
        4 + 24 + 8
    }
}

impl Key for Vec<u8> {
    fn write_key(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
    fn from_key_bytes(b: &[u8]) -> Self {
        b.to_vec()
    }
    fn key_heap_bytes(&self) -> usize {
        self.len() + 24 + 24 + 8
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// One worker's accumulator. `push` is the hot path; `spill_if_full` is called
/// only between files, so a file's postings are never split across a spill and
/// the per-file dedup in the caller stays correct.
pub struct Sink<K: Key> {
    map: HashMap<K, Vec<u32>>,
    bytes: usize,
    budget: usize,
    capacity: usize,
    dir: PathBuf,
    tag: &'static str,
    worker: usize,
    segments: Vec<PathBuf>,
}

impl<K: Key> Sink<K> {
    /// `capacity` is the map's initial slot count. It is worth setting per key
    /// type: one accumulator exists per chunk, so an over-generous word map
    /// (its slots are twice a gram's, and its keys allocate) costs that much
    /// again on every chunk of every build, including the small trees that
    /// never approach the budget.
    pub fn new(
        dir: &Path,
        tag: &'static str,
        worker: usize,
        budget: usize,
        capacity: usize,
    ) -> Self {
        Sink {
            map: HashMap::with_capacity(capacity),
            bytes: 0,
            budget,
            capacity,
            dir: dir.to_path_buf(),
            tag,
            worker,
            segments: Vec::new(),
        }
    }

    /// Record that `file` holds `key`. Repeating the most recent id for a key
    /// is ignored, which is how the caller deduplicates within one file.
    ///
    /// One `entry` rather than a `get_mut` followed by an `insert`: a miss is
    /// common while a chunk's key set is still filling, and this is the inner
    /// loop over every gram of every file. Cloning the key is why this is for
    /// `Copy`-cheap keys; byte keys go through [`Sink::push_bytes`].
    #[inline]
    pub fn push(&mut self, key: &K, file: u32) {
        let mut delta = 0usize;
        match self.map.entry(key.clone()) {
            hashbrown::hash_map::Entry::Occupied(mut e) => {
                let v = e.get_mut();
                if v.last() != Some(&file) {
                    // a `Vec` doubles, so charge the amortized growth, not the push
                    if v.len() == v.capacity() {
                        delta = 4 * v.capacity().max(1);
                    }
                    v.push(file);
                }
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                delta = key.key_heap_bytes() + 4 * 4;
                let mut v = Vec::with_capacity(4);
                v.push(file);
                e.insert(v);
            }
        }
        self.bytes += delta;
    }

    pub fn held_bytes(&self) -> usize {
        self.bytes
    }

    /// Write the accumulated postings out if the budget is reached. Safe to
    /// call only between files.
    pub fn spill_if_full(&mut self) -> Result<()> {
        if self.bytes >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        if self.map.is_empty() {
            return Ok(());
        }
        let path = self.dir.join(format!(
            "{}-{}-{}.seg",
            self.tag,
            self.worker,
            self.segments.len()
        ));
        let mut keys: Vec<K> = self.map.keys().cloned().collect();
        keys.sort_unstable();
        let f = crate::private_file()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        let mut w = BufWriter::with_capacity(256 << 10, f);
        let mut rec = Vec::with_capacity(4096);
        for k in &keys {
            let v = self.map.get_mut(k).expect("key from this map");
            v.sort_unstable();
            v.dedup();
            rec.clear();
            let mut kb = Vec::with_capacity(16);
            k.write_key(&mut kb);
            put_varint(&mut rec, kb.len() as u64);
            rec.extend_from_slice(&kb);
            put_varint(&mut rec, v.len() as u64);
            let mut prev = 0u32;
            for (i, &id) in v.iter().enumerate() {
                put_varint(
                    &mut rec,
                    if i == 0 {
                        id as u64
                    } else {
                        (id - prev) as u64
                    },
                );
                prev = id;
            }
            w.write_all(&rec)?;
        }
        w.flush()?;
        drop(w);
        // release the allocation rather than keeping the capacity: the point
        // of the spill is that the memory goes back
        self.map = HashMap::with_capacity(self.capacity);
        self.bytes = 0;
        self.segments.push(path);
        Ok(())
    }

    /// Hand back what this worker accumulated: its map when it never spilled,
    /// its segments (including a final spill) when it did.
    pub fn finish(mut self) -> Result<Part<K>> {
        if self.segments.is_empty() {
            return Ok(Part::Memory(self.map));
        }
        self.spill()?;
        Ok(Part::Segments(self.segments))
    }
}

impl Sink<Vec<u8>> {
    /// `push` for a borrowed byte key: the word is copied into the map only
    /// when it is new, which is the rare case. This is the hottest loop of
    /// phase 1 (every word of every file), so it does not go through `&Vec`.
    #[inline]
    pub fn push_bytes(&mut self, key: &[u8], file: u32) {
        match self.map.get_mut(key) {
            Some(v) => {
                if v.last() != Some(&file) {
                    if v.len() == v.capacity() {
                        self.bytes += 4 * v.capacity().max(1);
                    }
                    v.push(file);
                }
            }
            None => {
                self.bytes += key.len() + 56 + 4 * 4;
                let mut v = Vec::with_capacity(4);
                v.push(file);
                self.map.insert(key.to_vec(), v);
            }
        }
    }
}

pub enum Part<K: Key> {
    Memory(HashMap<K, Vec<u32>>),
    Segments(Vec<PathBuf>),
}

/// Every worker's contribution, in whichever shape they ended up.
pub enum Postings<K: Key> {
    /// No worker spilled: the pre-existing in-memory path, unchanged.
    Memory(Vec<HashMap<K, Vec<u32>>>),
    /// At least one worker spilled, so all of them did.
    Segments(Vec<PathBuf>),
}

/// Collect worker parts. If any worker spilled, the ones that did not are
/// written out too, so the merge has one kind of input rather than two.
pub fn collect<K: Key>(parts: Vec<Part<K>>, dir: &Path, tag: &'static str) -> Result<Postings<K>> {
    if parts.iter().all(|p| matches!(p, Part::Memory(_))) {
        return Ok(Postings::Memory(
            parts
                .into_iter()
                .map(|p| match p {
                    Part::Memory(m) => m,
                    Part::Segments(_) => unreachable!(),
                })
                .collect(),
        ));
    }
    let mut segments = Vec::new();
    for (i, p) in parts.into_iter().enumerate() {
        match p {
            Part::Segments(s) => segments.extend(s),
            Part::Memory(m) => {
                let mut s: Sink<K> = Sink::new(dir, tag, 1_000_000 + i, usize::MAX, 0);
                s.map = m;
                s.bytes = usize::MAX;
                s.spill()?;
                segments.extend(s.segments);
            }
        }
    }
    Ok(Postings::Segments(segments))
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

struct Reader {
    f: File,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    key: Vec<u8>,
    files: Vec<u32>,
}

impl Reader {
    fn open(path: &Path, cap: usize) -> Result<Option<Self>> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut r = Reader {
            f,
            buf: vec![0u8; cap],
            pos: 0,
            len: 0,
            eof: false,
            key: Vec::new(),
            files: Vec::new(),
        };
        Ok(if r.advance()? { Some(r) } else { None })
    }

    fn fill(&mut self) -> Result<()> {
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.len, 0);
            self.len -= self.pos;
            self.pos = 0;
        }
        if self.len == self.buf.len() {
            self.buf.resize(self.buf.len() * 2, 0);
        }
        let n = self.f.read(&mut self.buf[self.len..])?;
        if n == 0 {
            self.eof = true;
        }
        self.len += n;
        Ok(())
    }

    fn varint(&mut self) -> Result<Option<u64>> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            if self.pos == self.len {
                if self.eof {
                    return Ok(None);
                }
                self.fill()?;
                if self.pos == self.len && self.eof {
                    return Ok(None);
                }
                continue;
            }
            let b = self.buf[self.pos];
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(Some(v));
            }
            shift += 7;
            anyhow::ensure!(shift < 64, "corrupt build segment: varint too long");
        }
    }

    fn take(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.reserve(n);
        let mut left = n;
        while left > 0 {
            if self.pos == self.len {
                self.fill()?;
                anyhow::ensure!(self.pos < self.len, "corrupt build segment: truncated");
            }
            let take = left.min(self.len - self.pos);
            out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            left -= take;
        }
        Ok(())
    }

    /// Read the next record into `key`/`files`. False at a clean end of file.
    fn advance(&mut self) -> Result<bool> {
        let Some(klen) = self.varint()? else {
            return Ok(false);
        };
        let mut key = std::mem::take(&mut self.key);
        self.take(klen as usize, &mut key)?;
        self.key = key;
        let n = self
            .varint()?
            .context("corrupt build segment: record without a count")?;
        self.files.clear();
        self.files.reserve(n as usize);
        let mut prev = 0u32;
        for i in 0..n {
            let d = self
                .varint()?
                .context("corrupt build segment: record short of ids")? as u32;
            let id = if i == 0 { d } else { prev + d };
            self.files.push(id);
            prev = id;
        }
        Ok(true)
    }
}

/// Streaming k-way merge over the segments: yields every distinct key once, in
/// ascending order, with its file ids sorted and deduplicated.
pub struct Merger {
    readers: Vec<Reader>,
    heap: BinaryHeap<Reverse<(Vec<u8>, usize)>>,
    files: Vec<u32>,
}

impl Merger {
    fn open(segments: &[PathBuf]) -> Result<Self> {
        let cap = (MERGE_READ_BUDGET / segments.len().max(1)).max(MIN_SEGMENT_BUF);
        let mut readers = Vec::with_capacity(segments.len());
        let mut heap = BinaryHeap::new();
        for p in segments {
            if let Some(r) = Reader::open(p, cap)? {
                heap.push(Reverse((r.key.clone(), readers.len())));
                readers.push(r);
            }
        }
        Ok(Merger {
            readers,
            heap,
            files: Vec::new(),
        })
    }

    /// The next key and its postings, or `None` when every segment is drained.
    fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u32>)>> {
        let Some(Reverse((key, _))) = self.heap.peek().cloned() else {
            return Ok(None);
        };
        self.files.clear();
        while let Some(Reverse((k, i))) = self.heap.peek().cloned() {
            if k != key {
                break;
            }
            self.heap.pop();
            self.files.extend_from_slice(&self.readers[i].files);
            if self.readers[i].advance()? {
                self.heap.push(Reverse((self.readers[i].key.clone(), i)));
            }
        }
        self.files.sort_unstable();
        self.files.dedup();
        Ok(Some((key, std::mem::take(&mut self.files))))
    }
}

/// Merge `segments` and hand batches of at most `batch` entries to `f`, in
/// ascending key order. Batching keeps the caller's serialization parallel
/// while the merge itself stays streaming.
pub fn stream_batches<K: Key>(
    segments: &[PathBuf],
    batch: usize,
    mut f: impl FnMut(Vec<(K, Vec<u32>)>) -> Result<()>,
) -> Result<()> {
    let mut m = Merger::open(segments)?;
    let mut acc: Vec<(K, Vec<u32>)> = Vec::with_capacity(batch);
    while let Some((k, v)) = m.next()? {
        acc.push((K::from_key_bytes(&k), v));
        if acc.len() == batch {
            f(std::mem::take(&mut acc))?;
            acc.reserve(batch);
        }
    }
    if !acc.is_empty() {
        f(acc)?;
    }
    Ok(())
}

/// A directory for a build's spill segments, removed when the build ends —
/// including when it ends by `?`.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(index_dir: &Path) -> Result<Self> {
        let dir = index_dir.join(format!("build-tmp.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Scratch { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain<K: Key>(segments: &[PathBuf]) -> Vec<(K, Vec<u32>)> {
        let mut out = Vec::new();
        stream_batches::<K>(segments, 3, |b| {
            out.extend(b);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn spilled_postings_equal_the_in_memory_ones() {
        let tmp = std::env::temp_dir().join(format!("greeg-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // deterministic pseudo-random (gram, file) stream
        let mut want: std::collections::BTreeMap<u32, Vec<u32>> = Default::default();
        let mut s: Sink<u32> = Sink::new(&tmp, "g", 0, 4096, 1 << 10);
        let mut x: u64 = 12345;
        for file in 0..500u32 {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..40 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                let g = ((x >> 33) % 97) as u32;
                seen.insert(g);
            }
            for g in seen {
                s.push(&g, file);
                want.entry(g).or_default().push(file);
            }
            s.spill_if_full().unwrap();
        }
        let Part::Segments(segs) = s.finish().unwrap() else {
            panic!("the 4 KiB budget must have forced a spill");
        };
        assert!(segs.len() > 1, "expected several segments, got {segs:?}");
        let got: Vec<(u32, Vec<u32>)> = drain(&segs);
        let want: Vec<(u32, Vec<u32>)> = want.into_iter().collect();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn word_keys_round_trip_in_byte_order() {
        let tmp = std::env::temp_dir().join(format!("greeg-extw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut s: Sink<Vec<u8>> = Sink::new(&tmp, "w", 0, 1, 1 << 10);
        for (i, w) in ["zeta", "alpha", "alpha", "Beta", "alphabet"]
            .iter()
            .enumerate()
        {
            s.push(&w.as_bytes().to_vec(), i as u32);
            s.spill_if_full().unwrap();
        }
        let Part::Segments(segs) = s.finish().unwrap() else {
            panic!("spill expected");
        };
        let got: Vec<(Vec<u8>, Vec<u32>)> = drain(&segs);
        let keys: Vec<String> = got
            .iter()
            .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
            .collect();
        assert_eq!(keys, ["Beta", "alpha", "alphabet", "zeta"]);
        assert_eq!(got[1].1, vec![1, 2], "one key, two files, merged in order");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_sink_under_budget_never_spills() {
        let tmp = std::env::temp_dir().join(format!("greeg-extm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut s: Sink<u32> = Sink::new(&tmp, "g", 0, 1 << 30, 1 << 10);
        for f in 0..100u32 {
            s.push(&(f % 7), f);
            s.spill_if_full().unwrap();
        }
        assert!(matches!(s.finish().unwrap(), Part::Memory(_)));
        assert_eq!(
            std::fs::read_dir(&tmp).unwrap().count(),
            0,
            "no segment written"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
