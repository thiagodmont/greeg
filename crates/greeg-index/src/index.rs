//! Read side: mmap the components and answer gram, symbol and span queries.

use crate::format::{self, FileRec, FilesView, GramsView, ImpRec, NONE, SymRec};
use crate::plan::Q;
use crate::symtab::{DeltaGraphView, GraphView, SpansView, SymbolsView};
use crate::words::WordsView;
use crate::{Manifest, read_manifest};
use anyhow::{Context, Result, bail};
use greeg_lang::Lang;
use hashbrown::HashMap;
use memmap2::{Advice, Mmap};
use roaring::RoaringBitmap;
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct Segment {
    /// Maps backing the views below (self-referential by construction; never
    /// moved out). The `'static` is internal: every accessor rebinds the view
    /// to `&self`.
    _maps: Vec<Mmap>,
    files: FilesView<'static>,
    grams: GramsView<'static>,
    symbols: Option<SymbolsView<'static>>,
    spans: Option<SpansView<'static>>,
    /// Word postings (ARCHITECTURE.md): whole-word queries read these instead of the grams.
    words: Option<WordsView<'static>>,
    /// Import edges and superseded ids (delta segments only).
    dgraph: Option<DeltaGraphView<'static>>,
    pub first_id: u32,
    pub n_files: u32,
    /// Absolute ids of files above `MAX_FILE`: no grams, always candidates.
    huge: RoaringBitmap,
    /// Absolute ids of tracked-only files (ignore files): never candidates.
    hidden: RoaringBitmap,
    corrupt: std::sync::atomic::AtomicBool,
}

fn mmap(path: &Path, advice: Advice) -> Result<Mmap> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: index files are published atomically and only replaced by rename;
    // a truncation during read would SIGBUS, which the caller guards by not
    // truncating in place (ARCHITECTURE.md).
    let m = unsafe { Mmap::map(&f)? };
    let _ = m.advise(advice);
    Ok(m)
}

fn leak<'a>(b: &'a [u8]) -> &'static [u8] {
    // SAFETY: the Mmap is stored alongside and dropped after these views.
    unsafe { std::mem::transmute::<&'a [u8], &'static [u8]>(b) }
}

fn ids_bitmap(first_id: u32, local: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &i in local {
        b.insert(first_id + i);
    }
    b
}

/// Parse a delta body: header, five sections, tombstone bitmap. Every slice is
/// checked so a truncated or corrupt file fails `Index::open` (→ rebuild)
/// instead of panicking or silently dropping tombstones.
struct DeltaParts {
    files: FilesView<'static>,
    grams: GramsView<'static>,
    symbols: Option<SymbolsView<'static>>,
    spans: Option<SpansView<'static>>,
    dgraph: Option<DeltaGraphView<'static>>,
    words: Option<WordsView<'static>>,
    first_id: u32,
    tomb: RoaringBitmap,
}

fn parse_delta(body: &'static [u8]) -> Result<DeltaParts> {
    let hb = body
        .get(..format::DELTA_HEADER)
        .context("delta header truncated")?;
    let h: Vec<usize> = (0..8)
        .map(|i| u32::from_le_bytes(hb[i * 4..i * 4 + 4].try_into().unwrap()) as usize)
        .collect();
    let first_id = h[0] as u32;
    let mut off: usize = format::DELTA_HEADER;
    let mut section = |len: usize, what: &str| -> Result<&'static [u8]> {
        let b = body
            .get(off..off.checked_add(len).context("delta section overflow")?)
            .with_context(|| format!("delta {what} section truncated"))?;
        off += len;
        Ok(b)
    };
    let fb = section(h[2], "files")?;
    let gb = section(h[3], "grams")?;
    let sb = section(h[4], "symbols")?;
    let pb = section(h[5], "spans")?;
    let db = section(h[6], "graph")?;
    let wb = section(h[7], "words")?;
    let tb = body.get(off..).context("delta tombstones truncated")?;
    let tomb = RoaringBitmap::deserialize_from(tb).context("delta tombstone bitmap corrupt")?;
    let files = FilesView::parse(fb)?;
    if files.files.len() != h[1] {
        bail!("delta file count mismatch");
    }
    let grams = GramsView::parse(gb)?;
    let symbols = if sb.is_empty() {
        None
    } else {
        Some(SymbolsView::parse(sb)?)
    };
    let spans = if pb.is_empty() {
        None
    } else {
        Some(SpansView::parse(pb)?)
    };
    let dgraph = if db.is_empty() {
        None
    } else {
        let g = DeltaGraphView::parse(db)?;
        if g.n as usize != h[1] {
            bail!("delta graph file count mismatch");
        }
        Some(g)
    };
    let words = if wb.is_empty() {
        None
    } else {
        Some(WordsView::parse(wb)?)
    };
    Ok(DeltaParts {
        files,
        grams,
        symbols,
        spans,
        dgraph,
        words,
        first_id,
        tomb,
    })
}

impl Segment {
    pub fn files(&self) -> &FilesView<'_> {
        &self.files
    }
    pub fn grams(&self) -> &GramsView<'_> {
        &self.grams
    }
    pub fn symbols(&self) -> Option<&SymbolsView<'_>> {
        self.symbols.as_ref()
    }
    pub fn spans(&self) -> Option<&SpansView<'_>> {
        self.spans.as_ref()
    }
    pub fn rec(&self, id: u32) -> Option<&FileRec> {
        self.files
            .files
            .get(id.checked_sub(self.first_id)? as usize)
    }
    pub fn path(&self, id: u32) -> Option<&str> {
        self.rec(id).map(|r| self.files.path(r))
    }
    /// Every searchable id of the segment (tracked-only files excluded).
    pub fn all(&self) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        b.insert_range(self.first_id..self.first_id + self.n_files);
        b -= &self.hidden;
        b
    }
    pub fn huge(&self) -> &RoaringBitmap {
        &self.huge
    }
    pub fn hidden(&self) -> &RoaringBitmap {
        &self.hidden
    }
    /// Posting list for a gram; `None` when the gram is absent. A list that
    /// fails to deserialize marks the segment corrupt and reads as "every
    /// file", which keeps the candidate set a superset.
    fn posting(&self, key: u32) -> Option<(u32, RoaringBitmap)> {
        let i = self.grams.find(key)?;
        let bytes = self.grams.posting_bytes(i);
        match RoaringBitmap::deserialize_from(bytes) {
            Ok(bm) => Some((self.grams.counts[i], bm)),
            Err(_) => {
                self.corrupt
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Some((self.n_files, self.all()))
            }
        }
    }
    pub fn corrupt(&self) -> bool {
        self.corrupt.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Union of the word postings of `alts` (ARCHITECTURE.md); `None` when this
    /// segment has no word section. An absent word contributes nothing; an
    /// unreadable posting list reads as every file, as for grams.
    pub fn eval_words(&self, alts: &[Vec<u8>]) -> Option<RoaringBitmap> {
        let wv = self.words.as_ref()?;
        let mut acc = RoaringBitmap::new();
        for w in alts {
            let Some(i) = wv.find(w) else { continue };
            match RoaringBitmap::deserialize_from(wv.posting_bytes(i)) {
                Ok(bm) => acc |= bm,
                Err(_) => {
                    self.corrupt
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    return Some(self.all());
                }
            }
        }
        Some(acc)
    }
    /// Evaluate a plan against this segment. `None` = every file (All).
    pub fn eval(&self, q: &Q) -> Option<RoaringBitmap> {
        match q {
            Q::All => None,
            Q::None => Some(RoaringBitmap::new()),
            Q::Gram(g) => Some(self.posting(*g).map(|(_, b)| b).unwrap_or_default()),
            Q::And(v) => {
                // rarest first; grams that prune nothing are skipped (ARCHITECTURE.md)
                let total = self.n_files.max(1);
                let mut lists: Vec<(u32, u32)> = Vec::new(); // (count, key)
                let mut subs: Vec<&Q> = Vec::new();
                for x in v {
                    match x {
                        Q::Gram(g) => match self.grams.find(*g) {
                            Some(i) => lists.push((self.grams.counts[i], *g)),
                            None => return Some(RoaringBitmap::new()),
                        },
                        other => subs.push(other),
                    }
                }
                lists.sort_unstable();
                let mut acc: Option<RoaringBitmap> = None;
                for (n, (count, key)) in lists.iter().enumerate() {
                    if n >= 6 || (n > 0 && *count as u64 * 2 > total as u64) {
                        break;
                    }
                    let (_, bm) = self.posting(*key)?;
                    match acc.as_mut() {
                        None => acc = Some(bm),
                        Some(a) => *a &= bm,
                    }
                    if acc.as_ref().map(|a| a.is_empty()).unwrap_or(false) {
                        return acc;
                    }
                }
                for s in subs {
                    if let Some(bm) = self.eval(s) {
                        match acc.as_mut() {
                            None => acc = Some(bm),
                            Some(a) => *a &= bm,
                        }
                    }
                }
                acc
            }
            Q::Or(v) => {
                let mut acc = RoaringBitmap::new();
                for x in v {
                    acc |= self.eval(x)?;
                }
                Some(acc)
            }
        }
    }
}

/// A symbol reference: which segment, and the segment-local symbol id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SymId {
    pub seg: u8,
    pub idx: u32,
}

pub struct Index {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub base: Segment,
    pub deltas: Vec<Segment>,
    pub tomb: RoaringBitmap,
    graph: OnceLock<Option<(Mmap, GraphView<'static>)>>,
    /// Delta file id → id of the version it superseded (from the delta graph sections).
    prev: HashMap<u32, u32>,
    /// Superseded id → the delta file id that replaced it.
    next: HashMap<u32, u32>,
    /// The `skipped` record, mapped at open like every component, so a
    /// rebuild that removes it does not leave this reader without it.
    skipped: Option<Mmap>,
}

impl Index {
    /// Whether this index was built for `root`: a directory shared by two
    /// roots (`--index-dir`) never answers for, or takes changes from, the other.
    pub fn built_for(&self, root: &Path) -> bool {
        crate::RootId::of(root).is_some_and(|r| r == self.manifest.root_id)
    }

    /// Freshness ignores inode numbers on this index's file system.
    pub fn no_ino(&self) -> bool {
        self.manifest.stamp_mode == crate::STAMP_NO_INO
    }

    pub fn open(dir: &Path) -> Result<Index> {
        let manifest = read_manifest(dir).context("no usable index manifest")?;
        if !manifest.phase1 {
            bail!("index phase 1 not complete");
        }
        let generation = manifest.generation;
        // the file table is read for every candidate; postings are touched sparsely
        let fmap = mmap(
            &dir.join(format!("files.{generation}.bin")),
            Advice::WillNeed,
        )?;
        let gmap = mmap(&dir.join(format!("grams.{generation}.bin")), Advice::Random)?;
        let wmap = mmap(&dir.join(format!("words.{generation}.bin")), Advice::Random)?;
        let fbody = leak(format::check_header(&fmap, format::COMP_FILES)?);
        let gbody = leak(format::check_header(&gmap, format::COMP_GRAMS)?);
        let wbody = leak(format::check_header(&wmap, format::COMP_WORDS)?);
        let files = FilesView::parse(fbody)?;
        let grams = GramsView::parse(gbody)?;
        let words = Some(WordsView::parse(wbody)?);
        let n = files.files.len() as u32;
        let mut maps = vec![fmap, gmap, wmap];
        let (mut symbols, mut spans) = (None, None);
        if manifest.phase2 {
            let smap = mmap(
                &dir.join(format!("symbols.{generation}.bin")),
                Advice::Random,
            )?;
            let pmap = mmap(&dir.join(format!("spans.{generation}.bin")), Advice::Random)?;
            symbols = Some(SymbolsView::parse(leak(format::check_header(
                &smap,
                format::COMP_SYMBOLS,
            )?))?);
            spans = Some(SpansView::parse(leak(format::check_header(
                &pmap,
                format::COMP_SPANS,
            )?))?);
            maps.push(smap);
            maps.push(pmap);
        }
        let huge = ids_bitmap(0, files.huge);
        let hidden = ids_bitmap(0, files.hidden);
        let base = Segment {
            _maps: maps,
            files,
            grams,
            symbols,
            spans,
            words,
            dgraph: None,
            first_id: 0,
            n_files: n,
            huge,
            hidden,
            corrupt: Default::default(),
        };
        // only the deltas the manifest names, in order; stray files (from a
        // superseded generation or an interrupted writer) are ignored
        let mut deltas = Vec::with_capacity(manifest.deltas as usize);
        let mut tomb = RoaringBitmap::new();
        let mut next = n;
        let mut prev_map: HashMap<u32, u32> = HashMap::new();
        let mut next_map: HashMap<u32, u32> = HashMap::new();
        for i in 1..=manifest.deltas {
            let p = dir.join("delta").join(format!("{i:04}.bin"));
            let map = mmap(&p, Advice::WillNeed)?;
            let body = leak(
                format::check_header(&map, format::COMP_DELTA)
                    .with_context(|| format!("delta {}", p.display()))?,
            );
            let DeltaParts {
                files,
                grams,
                symbols,
                spans,
                dgraph,
                words,
                first_id,
                tomb: t,
            } = parse_delta(body).with_context(|| format!("delta {}", p.display()))?;
            if first_id != next {
                bail!(
                    "delta {} starts at id {first_id}, expected {next}",
                    p.display()
                );
            }
            let n = files.files.len() as u32;
            next = first_id + n;
            tomb |= t;
            if let Some(g) = &dgraph {
                for (local, &old) in g.prev.iter().enumerate() {
                    if old != NONE {
                        let id = first_id + local as u32;
                        prev_map.insert(id, old);
                        next_map.insert(old, id);
                    }
                }
            }
            let huge = ids_bitmap(first_id, files.huge);
            let hidden = ids_bitmap(first_id, files.hidden);
            deltas.push(Segment {
                _maps: vec![map],
                files,
                grams,
                symbols,
                spans,
                words,
                dgraph,
                first_id,
                n_files: n,
                huge,
                hidden,
                corrupt: Default::default(),
            });
        }
        let skipped = crate::skipped::path(dir, &manifest.skipped)
            .and_then(|p| mmap(&p, Advice::Sequential).ok());
        Ok(Index {
            dir: dir.to_path_buf(),
            manifest,
            base,
            deltas,
            tomb,
            graph: OnceLock::new(),
            prev: prev_map,
            next: next_map,
            skipped,
        })
    }

    /// Next free file id for a new delta segment.
    pub fn next_id(&self) -> u32 {
        self.deltas
            .last()
            .map(|d| d.first_id + d.n_files)
            .unwrap_or(self.base.n_files)
    }

    pub fn segment_for(&self, id: u32) -> Option<&Segment> {
        if id < self.base.n_files {
            return Some(&self.base);
        }
        self.deltas
            .iter()
            .find(|d| id >= d.first_id && id < d.first_id + d.n_files)
    }
    /// Segment index (0 = base) for a file id.
    pub fn seg_index(&self, id: u32) -> Option<u8> {
        if id < self.base.n_files {
            return Some(0);
        }
        self.deltas
            .iter()
            .position(|d| id >= d.first_id && id < d.first_id + d.n_files)
            .map(|i| i as u8 + 1)
    }
    pub fn segment(&self, seg: u8) -> &Segment {
        if seg == 0 {
            &self.base
        } else {
            &self.deltas[seg as usize - 1]
        }
    }
    pub fn segments(&self) -> impl Iterator<Item = (u8, &Segment)> {
        std::iter::once((0u8, &self.base)).chain(
            self.deltas
                .iter()
                .enumerate()
                .map(|(i, d)| (i as u8 + 1, d)),
        )
    }

    pub fn path(&self, id: u32) -> Option<&str> {
        self.segment_for(id)?.path(id)
    }
    pub fn rec(&self, id: u32) -> Option<&FileRec> {
        self.segment_for(id)?.rec(id)
    }
    pub fn is_live(&self, id: u32) -> bool {
        !self.tomb.contains(id) && self.segment_for(id).is_some()
    }
    /// What the walk left out, when this index recorded it.
    pub fn skipped(&self) -> Option<crate::skipped::Skipped> {
        crate::skipped::Skipped::from_file_bytes(self.skipped.as_deref()?)
    }

    /// Is `rel` a directory the index walked?
    pub fn has_dir(&self, rel: &str) -> bool {
        self.segments().any(|(_, seg)| {
            let fv = seg.files();
            fv.dirs.iter().any(|d| fv.dir_path(d) == rel)
        })
    }

    pub fn has_symbols(&self) -> bool {
        self.base.symbols.is_some()
    }
    /// Any segment hit an unreadable posting list during this query.
    pub fn corrupt(&self) -> bool {
        self.segments().any(|(_, s)| s.corrupt())
    }

    /// Live file ids matching the plan: (base ∪ deltas) minus tombstones.
    /// Huge files have no grams and are always included so verification opens
    /// them (and counts them when they exceed the query's size limit).
    pub fn candidates(&self, q: &Q) -> RoaringBitmap {
        self.candidates_with(q, None)
    }

    /// `candidates`, answering from the word postings wherever a segment has
    /// them when `words` names the whole-word alternatives of the pattern
    /// (`plan::word_plan`); the trigram plan covers the rest.
    pub fn candidates_with(&self, q: &Q, words: Option<&[Vec<u8>]>) -> RoaringBitmap {
        let seg_cands = |seg: &Segment| -> RoaringBitmap {
            if let Some(alts) = words
                && let Some(bm) = seg.eval_words(alts)
            {
                return bm;
            }
            seg.eval(q).unwrap_or_else(|| seg.all())
        };
        let mut acc = seg_cands(&self.base);
        acc |= &self.base.huge;
        for d in &self.deltas {
            acc |= seg_cands(d);
            acc |= &d.huge;
        }
        acc -= &self.tomb;
        acc
    }

    /// Live files holding any of `alts` as a whole word, from the word postings
    /// alone (no huge files, no trigram fallback): empty when no indexed file
    /// has the word.
    pub fn word_candidates(&self, alts: &[Vec<u8>]) -> RoaringBitmap {
        let mut acc = RoaringBitmap::new();
        for (_, seg) in self.segments() {
            if let Some(bm) = seg.eval_words(alts) {
                acc |= bm;
            }
        }
        acc -= &self.tomb;
        acc
    }

    /// Indexed words that contain `needle` as a strict substring, with their
    /// document counts, most files first: the `related` line of a bare
    /// identifier query, without opening a file. One `memmem` pass over each
    /// segment's dictionary.
    pub fn words_containing(&self, needle: &str, limit: usize) -> Vec<(String, usize)> {
        let nb = needle.as_bytes();
        if nb.is_empty() {
            return Vec::new();
        }
        let finder = memchr::memmem::Finder::new(nb);
        let mut counts: HashMap<&[u8], usize> = HashMap::new();
        for (_, seg) in self.segments() {
            let Some(wv) = seg.words.as_ref() else {
                continue;
            };
            for h in finder.find_iter(wv.arena) {
                let Some(i) = wv.word_at_offset(h) else {
                    continue;
                };
                // the hit must lie inside this word, and the word must be longer
                if h + nb.len() > wv.word_off[i + 1] as usize {
                    continue;
                }
                let w = wv.word(i);
                if w == nb {
                    continue;
                }
                *counts.entry(w).or_default() += wv.counts[i] as usize;
            }
        }
        let mut v: Vec<(String, usize)> = counts
            .into_iter()
            .map(|(w, c)| (String::from_utf8_lossy(w).into_owned(), c))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    }

    /// All live (searchable) file ids.
    pub fn live(&self) -> RoaringBitmap {
        self.candidates(&Q::All)
    }

    /// Number of live files, without materializing the set.
    pub fn live_count(&self) -> u64 {
        let mut dead = self.tomb.clone();
        for (_, seg) in self.segments() {
            dead |= &seg.hidden;
        }
        self.segments().map(|(_, s)| s.n_files as u64).sum::<u64>() - dead.len()
    }

    /// Iterate live (id, rel path, rec) in id order.
    pub fn live_files(&self) -> impl Iterator<Item = (u32, &str, &FileRec)> + '_ {
        self.live().into_iter().filter_map(move |id| {
            let seg = self.segment_for(id)?;
            let rec = seg.rec(id)?;
            Some((id, seg.files().path(rec), rec))
        })
    }

    /// Iterate every tracked, non-tombstoned file including tracked-only ones
    /// (ignore files), for the freshness check.
    pub fn tracked_files(&self) -> impl Iterator<Item = (u32, &str, &FileRec)> + '_ {
        self.segments().flat_map(move |(_, seg)| {
            let fv = seg.files();
            fv.files.iter().enumerate().filter_map(move |(i, rec)| {
                let id = seg.first_id + i as u32;
                if self.tomb.contains(id) {
                    None
                } else {
                    Some((id, fv.path(rec), rec))
                }
            })
        })
    }

    // ---------------------------------------------------------------- symbols

    /// Symbols of a live file: (segment-local first symbol id, records).
    pub fn symbols_of(&self, id: u32) -> Option<(SymId, &[SymRec])> {
        let seg_i = self.seg_index(id)?;
        let seg = self.segment(seg_i);
        let sv = seg.symbols.as_ref()?;
        let (base, syms) = sv.symbols_of(id - seg.first_id);
        Some((
            SymId {
                seg: seg_i,
                idx: base,
            },
            syms,
        ))
    }
    pub fn sym(&self, s: SymId) -> Option<&SymRec> {
        self.segment(s.seg)
            .symbols
            .as_ref()?
            .syms
            .get(s.idx as usize)
    }
    pub fn sym_name(&self, s: SymId) -> &str {
        let seg = self.segment(s.seg);
        match (seg.symbols.as_ref(), self.sym(s)) {
            (Some(sv), Some(r)) => sv.name(r.name_id),
            _ => "",
        }
    }
    /// Absolute file id of a symbol.
    pub fn sym_file(&self, s: SymId) -> u32 {
        let seg = self.segment(s.seg);
        self.sym(s).map(|r| seg.first_id + r.file).unwrap_or(NONE)
    }
    pub fn sym_parent(&self, s: SymId) -> Option<SymId> {
        let r = self.sym(s)?;
        if r.parent == NONE {
            None
        } else {
            Some(SymId {
                seg: s.seg,
                idx: r.parent,
            })
        }
    }
    /// Names of supertypes of a symbol.
    pub fn sym_supers(&self, s: SymId) -> Vec<&str> {
        let seg = self.segment(s.seg);
        match (seg.symbols.as_ref(), self.sym(s)) {
            (Some(sv), Some(r)) => sv.supers_of(r).iter().map(|&n| sv.name(n)).collect(),
            _ => Vec::new(),
        }
    }
    /// Enclosing chain (outermost first) as (kind code, name).
    pub fn sym_chain(&self, s: SymId) -> Vec<(u8, &str)> {
        let mut v = Vec::new();
        let mut cur = Some(s);
        let mut guard = 0;
        while let Some(c) = cur {
            if let Some(r) = self.sym(c) {
                v.push((r.kind, self.sym_name(c)));
            }
            cur = self.sym_parent(c);
            guard += 1;
            if guard > 16 {
                break;
            }
        }
        v.reverse();
        v
    }
    /// Live symbols with exactly this name, best first per segment (base first).
    pub fn lookup(&self, name: &str) -> Vec<SymId> {
        let mut out = Vec::new();
        for (si, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else {
                continue;
            };
            let Some(nid) = sv.find_name(name) else {
                continue;
            };
            for &idx in sv.syms_named(nid) {
                let file = seg.first_id + sv.syms[idx as usize].file;
                if !self.tomb.contains(file) {
                    out.push(SymId { seg: si, idx });
                }
            }
        }
        out
    }
    /// Innermost symbol containing byte `off` of file `id`.
    pub fn enclosing(&self, id: u32, off: u32) -> Option<SymId> {
        let seg_i = self.seg_index(id)?;
        let seg = self.segment(seg_i);
        let sv = seg.symbols.as_ref()?;
        sv.enclosing(id - seg.first_id, off)
            .map(|idx| SymId { seg: seg_i, idx })
    }
    /// Symbols whose supertypes include `name`.
    pub fn implementors(&self, name: &str) -> Vec<SymId> {
        let mut out = Vec::new();
        for (si, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else {
                continue;
            };
            let Some(nid) = sv.find_name(name) else {
                continue;
            };
            for (idx, r) in sv.syms.iter().enumerate() {
                if r.super_len > 0
                    && sv.supers_of(r).contains(&nid)
                    && !self.tomb.contains(seg.first_id + r.file)
                {
                    out.push(SymId {
                        seg: si,
                        idx: idx as u32,
                    });
                }
            }
        }
        out
    }
    /// Name ids across segments that contain every split token of `tokens`; returns names.
    pub fn names_with_tokens(&self, tokens: &[String], limit: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else {
                continue;
            };
            let mut acc: Option<Vec<u32>> = None;
            for t in tokens {
                let Some(ids) = sv.find_token(t) else {
                    acc = Some(Vec::new());
                    break;
                };
                acc = Some(match acc {
                    None => ids.to_vec(),
                    Some(a) => a
                        .iter()
                        .copied()
                        .filter(|x| ids.binary_search(x).is_ok())
                        .collect(),
                });
            }
            for nid in acc.unwrap_or_default() {
                let n = sv.name(nid).to_string();
                if !out.contains(&n) {
                    out.push(n);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
    /// Names within Levenshtein distance `d` of `q` (bounded scan over the sorted name table).
    pub fn fuzzy_names(&self, q: &str, d: usize, limit: usize) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        let ql = q.len();
        for (_, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else {
                continue;
            };
            for i in 0..sv.n_names() {
                let n = sv.name(i as u32);
                if n.len() + d < ql || n.len() > ql + d {
                    continue;
                }
                if let Some(dist) = levenshtein_bounded(q.as_bytes(), n.as_bytes(), d)
                    && dist > 0
                    && !out.iter().any(|(x, _)| x == n)
                {
                    out.push((n.to_string(), dist));
                }
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        out.truncate(limit);
        out
    }

    // ---------------------------------------------------------------- spans

    /// Noncode span at (file, offset): (kind code 0 comment / 1 string / 2 docstring, start, end).
    pub fn noncode_at(&self, id: u32, off: u32) -> Option<(u8, u32, u32)> {
        let seg = self.segment_for(id)?;
        seg.spans.as_ref()?.noncode_at(id - seg.first_id, off)
    }
    pub fn import_at(&self, id: u32, off: u32) -> Option<&ImpRec> {
        let seg = self.segment_for(id)?;
        seg.spans.as_ref()?.import_at(id - seg.first_id, off)
    }
    pub fn imports_of(&self, id: u32) -> &[ImpRec] {
        match self.segment_for(id) {
            Some(seg) => seg
                .spans
                .as_ref()
                .map(|s| s.imports_of(id - seg.first_id))
                .unwrap_or(&[]),
            None => &[],
        }
    }
    pub fn import_raw(&self, id: u32, i: &ImpRec) -> &str {
        match self.segment_for(id) {
            Some(seg) => seg.spans.as_ref().map(|s| s.raw(i)).unwrap_or(""),
            None => "",
        }
    }

    // ---------------------------------------------------------------- graph

    pub fn graph(&self) -> Option<&GraphView<'_>> {
        self.graph
            .get_or_init(|| {
                if !self.manifest.phase2 {
                    return None;
                }
                let map = mmap(
                    &self
                        .dir
                        .join(format!("graph.{}.bin", self.manifest.generation)),
                    Advice::WillNeed,
                )
                .ok()?;
                let body = leak(format::check_header(&map, format::COMP_GRAPH).ok()?);
                let view = GraphView::parse(body).ok()?;
                Some((map, view))
            })
            .as_ref()
            .map(|(_, v)| v)
    }
    /// The oldest id in a file's version chain: the base id for a file that was
    /// edited since the build, else the id itself. Ids on one chain name the
    /// same path.
    pub fn canon(&self, mut id: u32) -> u32 {
        let mut guard = 0;
        while let Some(&p) = self.prev.get(&id) {
            id = p;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        id
    }
    /// The newest id in a file's version chain (live unless the file was deleted).
    pub fn latest(&self, mut id: u32) -> u32 {
        let mut guard = 0;
        while let Some(&n) = self.next.get(&id) {
            id = n;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        id
    }
    /// Live files imported by `id`: base edges for base files, the delta's
    /// own resolved edges for edited files, every target mapped to its newest
    /// live version. Borrows the CSR slice when no delta has been applied.
    pub fn out_edges(&self, id: u32) -> Cow<'_, [u32]> {
        let raw: &[u32] = if id < self.base.n_files {
            self.graph().map(|g| g.out(id)).unwrap_or(&[])
        } else {
            match self.segment_for(id) {
                Some(seg) => seg
                    .dgraph
                    .as_ref()
                    .map(|g| g.out(id - seg.first_id))
                    .unwrap_or(&[]),
                None => &[],
            }
        };
        if self.deltas.is_empty() {
            return Cow::Borrowed(raw);
        }
        let mut v: Vec<u32> = raw
            .iter()
            .map(|&t| self.latest(t))
            .filter(|&t| self.is_live(t))
            .collect();
        v.sort_unstable();
        v.dedup();
        Cow::Owned(v)
    }
    /// Live files importing `id`: base importers of its oldest version that
    /// were not edited since (their edited versions carry their own edges),
    /// plus every delta file whose edges reach any version of `id`.
    pub fn in_edges(&self, id: u32) -> Cow<'_, [u32]> {
        let root = self.canon(id);
        let base_in: &[u32] = if root < self.base.n_files {
            self.graph().map(|g| g.incoming(root)).unwrap_or(&[])
        } else {
            &[]
        };
        if self.deltas.is_empty() {
            return Cow::Borrowed(base_in);
        }
        let mut v: Vec<u32> = base_in
            .iter()
            .copied()
            .filter(|&f| self.is_live(f))
            .collect();
        for d in &self.deltas {
            if let Some(g) = &d.dgraph {
                for (local, to) in g.edges() {
                    let from = d.first_id + local;
                    if self.canon(to) == root && self.is_live(from) {
                        v.push(from);
                    }
                }
            }
        }
        v.sort_unstable();
        v.dedup();
        Cow::Owned(v)
    }
    /// Live files that *are* the module `name`: `…/name.ext` in a language
    /// with a grammar, or a directory module `…/name/{mod.rs,__init__.py,index.*}`.
    /// SCIP and agents both treat the file as the module's definition, so
    /// `def` lists them (ARCHITECTURE.md). Generic stems never match. One
    /// `memmem` pass over each segment's path arena, no per-file work.
    pub fn module_files(&self, name: &str, limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        if name.is_empty()
            || matches!(name, "mod" | "index" | "__init__" | "lib" | "main")
            || name.bytes().any(|b| b == b'/' || b == b'.')
        {
            return out;
        }
        let nb = name.as_bytes();
        let finder = memchr::memmem::Finder::new(nb);
        for (_, seg) in self.segments() {
            let fv = seg.files();
            let arena = fv.arena;
            for h in finder.find_iter(arena) {
                let after = h + nb.len();
                let Some(&sep) = arena.get(after) else {
                    continue;
                };
                if sep != b'.' && sep != b'/' {
                    continue;
                }
                // the file record holding this offset (records are in path order,
                // so their arena offsets ascend; directory paths between them fail
                // the range check)
                let i = fv.files.partition_point(|r| (r.path_off as usize) <= h);
                if i == 0 {
                    continue;
                }
                let r = &fv.files[i - 1];
                let (ps, pe) = (
                    r.path_off as usize,
                    r.path_off as usize + r.path_len as usize,
                );
                if h >= pe || (h > ps && arena[h - 1] != b'/') {
                    continue;
                }
                let rest = &arena[after..pe];
                let ok = if sep == b'.' {
                    !rest.contains(&b'/') && Lang::from_path(Path::new(fv.path(r))).has_grammar()
                } else {
                    matches!(
                        &rest[1..],
                        b"mod.rs"
                            | b"__init__.py"
                            | b"index.ts"
                            | b"index.tsx"
                            | b"index.js"
                            | b"index.jsx"
                            | b"index.mjs"
                            | b"index.cjs"
                    )
                };
                if !ok {
                    continue;
                }
                let id = seg.first_id + (i as u32 - 1);
                if self.is_live(id) && !seg.hidden().contains(id) && !out.contains(&id) {
                    out.push(id);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
    /// Normalized PageRank of a file (0.5 when unknown).
    pub fn rank(&self, id: u32) -> f32 {
        match self.rec(id) {
            Some(r) if r.rank > 0 => (r.rank - 1) as f32 / 65534.0,
            _ => 0.5,
        }
    }
}

/// Levenshtein distance if ≤ `max`, else None (banded DP over bytes).
pub fn levenshtein_bounded(a: &[u8], b: &[u8], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    if d <= max { Some(d) } else { None }
}
