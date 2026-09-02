//! Read side: mmap the components and answer gram, symbol and span queries.

use crate::format::{self, FileRec, FilesView, GramsView, ImpRec, NONE, SymRec};
use crate::plan::Q;
use crate::symtab::{GraphView, SpansView, SymbolsView};
use crate::{Manifest, read_manifest};
use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use roaring::RoaringBitmap;
use std::sync::OnceLock;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Segment {
    /// Maps backing the views below (self-referential by construction; never moved out).
    _maps: Vec<Mmap>,
    files: FilesView<'static>,
    grams: GramsView<'static>,
    symbols: Option<SymbolsView<'static>>,
    spans: Option<SpansView<'static>>,
    pub first_id: u32,
    pub n_files: u32,
    corrupt: std::sync::atomic::AtomicBool,
}

fn mmap(path: &Path) -> Result<Mmap> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    // SAFETY: index files are published atomically and only replaced by rename;
    // a truncation during read would SIGBUS, which the caller guards by not
    // truncating in place (DESIGN.md §12).
    Ok(unsafe { Mmap::map(&f)? })
}

fn leak<'a>(b: &'a [u8]) -> &'static [u8] {
    // SAFETY: the Mmap is stored alongside and dropped after these views.
    unsafe { std::mem::transmute::<&'a [u8], &'static [u8]>(b) }
}

impl Segment {
    pub fn files(&self) -> &FilesView<'_> {
        &self.files
    }
    pub fn grams(&self) -> &GramsView<'_> {
        &self.grams
    }
    pub fn symbols(&self) -> Option<&SymbolsView<'static>> {
        self.symbols.as_ref()
    }
    pub fn spans(&self) -> Option<&SpansView<'static>> {
        self.spans.as_ref()
    }
    pub fn rec(&self, id: u32) -> Option<&FileRec> {
        self.files.files.get((id - self.first_id) as usize)
    }
    pub fn path(&self, id: u32) -> Option<&str> {
        self.rec(id).map(|r| self.files.path(r))
    }
    pub fn all(&self) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        b.insert_range(self.first_id..self.first_id + self.n_files);
        b
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
                self.corrupt.store(true, std::sync::atomic::Ordering::Relaxed);
                Some((self.n_files, self.all()))
            }
        }
    }
    pub fn corrupt(&self) -> bool {
        self.corrupt.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Evaluate a plan against this segment. `None` = every file (All).
    pub fn eval(&self, q: &Q) -> Option<RoaringBitmap> {
        match q {
            Q::All => None,
            Q::None => Some(RoaringBitmap::new()),
            Q::Gram(g) => Some(self.posting(*g).map(|(_, b)| b).unwrap_or_default()),
            Q::And(v) => {
                // rarest first; grams that prune nothing are skipped (DESIGN.md §5.2)
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
                    acc = Some(match acc {
                        None => bm,
                        Some(a) => a & bm,
                    });
                    if acc.as_ref().map(|a| a.is_empty()).unwrap_or(false) {
                        return acc;
                    }
                }
                for s in subs {
                    if let Some(bm) = self.eval(s) {
                        acc = Some(match acc {
                            None => bm,
                            Some(a) => a & bm,
                        });
                    }
                }
                acc
            }
            Q::Or(v) => {
                let mut acc = RoaringBitmap::new();
                for x in v {
                    match self.eval(x) {
                        None => return None,
                        Some(bm) => acc |= bm,
                    }
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
}

impl Index {
    pub fn open(dir: &Path) -> Result<Index> {
        let manifest = read_manifest(dir).context("no usable index manifest")?;
        if !manifest.phase1 {
            bail!("index phase 1 not complete");
        }
        let generation = manifest.generation;
        let fmap = mmap(&dir.join(format!("files.{generation}.bin")))?;
        let gmap = mmap(&dir.join(format!("grams.{generation}.bin")))?;
        let fbody = leak(format::check_header(&fmap, format::COMP_FILES)?);
        let gbody = leak(format::check_header(&gmap, format::COMP_GRAMS)?);
        let files = FilesView::parse(fbody)?;
        let grams = GramsView::parse(gbody)?;
        let n = files.files.len() as u32;
        let mut maps = vec![fmap, gmap];
        let (mut symbols, mut spans) = (None, None);
        if manifest.phase2 {
            let smap = mmap(&dir.join(format!("symbols.{generation}.bin")))?;
            let pmap = mmap(&dir.join(format!("spans.{generation}.bin")))?;
            symbols = Some(SymbolsView::parse(leak(format::check_header(&smap, format::COMP_SYMBOLS)?))?);
            spans = Some(SpansView::parse(leak(format::check_header(&pmap, format::COMP_SPANS)?))?);
            maps.push(smap);
            maps.push(pmap);
        }
        let base = Segment { _maps: maps, files, grams, symbols, spans, first_id: 0, n_files: n, corrupt: Default::default() };
        let mut deltas = Vec::new();
        let mut tomb = RoaringBitmap::new();
        if let Ok(rd) = fs::read_dir(dir.join("delta")) {
            let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "bin").unwrap_or(false)).collect();
            names.sort();
            for p in names {
                let map = mmap(&p)?;
                let body = leak(format::check_header(&map, format::COMP_DELTA)?);
                let h: Vec<u32> = (0..6).map(|i| u32::from_le_bytes(body[i * 4..i * 4 + 4].try_into().unwrap())).collect();
                let (first_id, fl, gl, sl, pl) = (h[0], h[2] as usize, h[3] as usize, h[4] as usize, h[5] as usize);
                let mut off = 24;
                let fb = &body[off..off + fl];
                off += fl;
                let gb = &body[off..off + gl];
                off += gl;
                let sb = &body[off..off + sl];
                off += sl;
                let pb = &body[off..off + pl];
                off += pl;
                let tb = &body[off..];
                if let Ok(t) = RoaringBitmap::deserialize_unchecked_from(tb) {
                    tomb |= t;
                }
                let files = FilesView::parse(fb)?;
                let grams = GramsView::parse(gb)?;
                let n = files.files.len() as u32;
                let symbols = if sl > 0 { Some(SymbolsView::parse(sb)?) } else { None };
                let spans = if pl > 0 { Some(SpansView::parse(pb)?) } else { None };
                deltas.push(Segment { _maps: vec![map], files, grams, symbols, spans, first_id, n_files: n, corrupt: Default::default() });
            }
        }
        Ok(Index { dir: dir.to_path_buf(), manifest, base, deltas, tomb, graph: OnceLock::new() })
    }

    /// Next free file id for a new delta segment.
    pub fn next_id(&self) -> u32 {
        self.deltas.last().map(|d| d.first_id + d.n_files).unwrap_or(self.base.n_files)
    }

    pub fn segment_for(&self, id: u32) -> Option<&Segment> {
        if id < self.base.n_files {
            return Some(&self.base);
        }
        self.deltas.iter().find(|d| id >= d.first_id && id < d.first_id + d.n_files)
    }
    /// Segment index (0 = base) for a file id.
    pub fn seg_index(&self, id: u32) -> Option<u8> {
        if id < self.base.n_files {
            return Some(0);
        }
        self.deltas.iter().position(|d| id >= d.first_id && id < d.first_id + d.n_files).map(|i| i as u8 + 1)
    }
    pub fn segment(&self, seg: u8) -> &Segment {
        if seg == 0 { &self.base } else { &self.deltas[seg as usize - 1] }
    }
    pub fn segments(&self) -> impl Iterator<Item = (u8, &Segment)> {
        std::iter::once((0u8, &self.base)).chain(self.deltas.iter().enumerate().map(|(i, d)| (i as u8 + 1, d)))
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
    pub fn has_symbols(&self) -> bool {
        self.base.symbols.is_some()
    }
    /// Any segment hit an unreadable posting list during this query.
    pub fn corrupt(&self) -> bool {
        self.segments().any(|(_, s)| s.corrupt())
    }

    /// Live file ids matching the plan: (base ∪ deltas) minus tombstones.
    pub fn candidates(&self, q: &Q) -> RoaringBitmap {
        let mut acc = self.base.eval(q).unwrap_or_else(|| self.base.all());
        for d in &self.deltas {
            acc |= d.eval(q).unwrap_or_else(|| d.all());
        }
        acc -= &self.tomb;
        acc
    }

    /// All live file ids.
    pub fn live(&self) -> RoaringBitmap {
        self.candidates(&Q::All)
    }

    /// Iterate live (id, rel path, rec) in id order.
    pub fn live_files(&self) -> impl Iterator<Item = (u32, &str, &FileRec)> + '_ {
        self.live().into_iter().filter_map(move |id| {
            let seg = self.segment_for(id)?;
            let rec = seg.rec(id)?;
            Some((id, seg.files().path(rec), rec))
        })
    }

    // ---------------------------------------------------------------- symbols

    /// Symbols of a live file: (segment-local first symbol id, records).
    pub fn symbols_of(&self, id: u32) -> Option<(SymId, &[SymRec])> {
        let seg_i = self.seg_index(id)?;
        let seg = self.segment(seg_i);
        let sv = seg.symbols.as_ref()?;
        let (base, syms) = sv.symbols_of(id - seg.first_id);
        Some((SymId { seg: seg_i, idx: base }, syms))
    }
    pub fn sym(&self, s: SymId) -> Option<&SymRec> {
        self.segment(s.seg).symbols.as_ref()?.syms.get(s.idx as usize)
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
        if r.parent == NONE { None } else { Some(SymId { seg: s.seg, idx: r.parent }) }
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
            let Some(sv) = seg.symbols.as_ref() else { continue };
            let Some(nid) = sv.find_name(name) else { continue };
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
        sv.enclosing(id - seg.first_id, off).map(|idx| SymId { seg: seg_i, idx })
    }
    /// Symbols whose supertypes include `name`.
    pub fn implementors(&self, name: &str) -> Vec<SymId> {
        let mut out = Vec::new();
        for (si, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else { continue };
            let Some(nid) = sv.find_name(name) else { continue };
            for (idx, r) in sv.syms.iter().enumerate() {
                if r.super_len > 0 && sv.supers_of(r).contains(&nid) && !self.tomb.contains(seg.first_id + r.file) {
                    out.push(SymId { seg: si, idx: idx as u32 });
                }
            }
        }
        out
    }
    /// Name ids across segments that contain every split token of `tokens`; returns names.
    pub fn names_with_tokens(&self, tokens: &[String], limit: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, seg) in self.segments() {
            let Some(sv) = seg.symbols.as_ref() else { continue };
            let mut acc: Option<Vec<u32>> = None;
            for t in tokens {
                let Some(ids) = sv.find_token(t) else {
                    acc = Some(Vec::new());
                    break;
                };
                acc = Some(match acc {
                    None => ids.to_vec(),
                    Some(a) => a.iter().copied().filter(|x| ids.binary_search(x).is_ok()).collect(),
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
            let Some(sv) = seg.symbols.as_ref() else { continue };
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
            Some(seg) => seg.spans.as_ref().map(|s| s.imports_of(id - seg.first_id)).unwrap_or(&[]),
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

    pub fn graph(&self) -> Option<&GraphView<'static>> {
        self.graph
            .get_or_init(|| {
                if !self.manifest.phase2 {
                    return None;
                }
                let map = mmap(&self.dir.join(format!("graph.{}.bin", self.manifest.generation))).ok()?;
                let body = leak(format::check_header(&map, format::COMP_GRAPH).ok()?);
                let view = GraphView::parse(body).ok()?;
                Some((map, view))
            })
            .as_ref()
            .map(|(_, v)| v)
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
