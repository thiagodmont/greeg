//! Phase-1 build (DESIGN.md §4.1): walk, read with a small reader pool,
//! extract trigrams, merge per-thread maps, write roaring postings, publish.

use crate::format::{self, DirRec, FileRec, FileTable, NONE, is_ignore_file};
use crate::gram::{Dedup, fold_buf};
use crate::resolve::Resolver;
use crate::symtab::{FileExtract, GraphBuilder, SpanBuilder, SymBuilder};
use crate::{Manifest, now_ms, read_manifest, write_manifest};
use anyhow::{Result, bail};
use greeg_lang::sym;
use greeg_lang::{FileFlags, Lang, content_flags, path_flags};
use hashbrown::HashMap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::fs;
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
}

impl Default for BuildOpts {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        BuildOpts { reader_threads: if cfg!(target_os = "macos") { cores.min(4) } else { cores }, quiet: true, phase1_only: false }
    }
}

#[derive(Clone, Debug)]
pub struct WalkedFile {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub dir: u32,
}

#[derive(Clone, Debug)]
pub struct WalkedDir {
    pub rel: String,
    pub mtime_ns: i64,
}

pub(crate) fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.mtime() * 1_000_000_000 + md.mtime_nsec()
}

/// A walker with scan-mode semantics (hidden skipped, ignore rules honoured)
/// that additionally yields ignore files (`.gitignore`, `.ignore`,
/// `.rgignore`) so the freshness check can see them change. They are stored
/// as tracked-only (`FileTable::hidden`) and never searched.
pub fn walker(path: &Path) -> ignore::WalkBuilder {
    let mut wb = ignore::WalkBuilder::new(path);
    wb.hidden(false).filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        !name.starts_with('.') || (e.file_type().map(|t| t.is_file()).unwrap_or(false) && is_ignore_file(&name))
    });
    wb
}

/// Walk `root` with the same ignore semantics as scan mode; returns files and
/// directories sorted by relative path, with the file's dir index resolved.
pub fn walk(root: &Path) -> Result<(Vec<WalkedFile>, Vec<WalkedDir>)> {
    type Files = Vec<(String, u64, i64)>;
    let out: Mutex<(Files, Vec<WalkedDir>)> = Mutex::new((Vec::with_capacity(4096), Vec::with_capacity(512)));
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
    let wb = walker(root).build_parallel();
    wb.run(|| {
        let mut local = Local { f: Vec::new(), d: Vec::new(), out: &out };
        Box::new(move |entry| {
            let Ok(e) = entry else { return ignore::WalkState::Continue };
            let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
            let Ok(md) = e.metadata() else { return ignore::WalkState::Continue };
            match e.file_type() {
                Some(t) if t.is_dir() => local.d.push(WalkedDir { rel, mtime_ns: mtime_ns(&md) }),
                Some(t) if t.is_file() => local.f.push((rel, md.len(), mtime_ns(&md))),
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
        let dir_index: HashMap<&str, u32> = dirs.iter().enumerate().map(|(i, d)| (d.rel.as_str(), i as u32)).collect();
        files
            .into_iter()
            .map(|(rel, size, mt)| {
                let dir = *dir_index.get(rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")).unwrap_or(&0);
                WalkedFile { rel, size, mtime_ns: mt, dir }
            })
            .collect()
    };
    Ok((files, dirs))
}

/// Per-file extraction record.
pub struct Extracted {
    pub flags: FileFlags,
    pub grams: Vec<u32>,
}

/// Read `path` (at most `MAX_FILE + 1` bytes) into `buf`, sized from the
/// walk's `size` so the read needs one allocation and no probing.
fn read_file(path: &Path, size: u64, buf: &mut Vec<u8>) -> bool {
    use std::io::Read;
    buf.clear();
    buf.reserve((size as usize).min(MAX_FILE as usize + 1) + 1);
    let Ok(f) = fs::File::open(path) else { return false };
    f.take(MAX_FILE + 1).read_to_end(buf).is_ok()
}

/// Read one file and extract its grams and flags. `buf` and `dd` are reused.
pub fn extract_file(path: &Path, rel: &str, size: u64, buf: &mut Vec<u8>, dd: &mut Dedup, grams: &mut Vec<u32>) -> Extracted {
    extract_file_with(path, rel, size, buf, dd, grams, |_, _| ()).0
}

/// `extract_file`, calling `hook(rel, bytes)` on the unfolded content before
/// gram extraction so a second extractor (symbols) needs no second read.
fn extract_file_with<T: Default>(path: &Path, rel: &str, size: u64, buf: &mut Vec<u8>, dd: &mut Dedup, grams: &mut Vec<u32>, hook: impl FnOnce(&str, &[u8]) -> T) -> (Extracted, T) {
    let mut flags = path_flags(rel);
    grams.clear();
    if size > MAX_FILE {
        flags.set(FileFlags::HUGE);
        return (Extracted { flags, grams: Vec::new() }, T::default());
    }
    if is_ignore_file(rel) || !read_file(path, size, buf) {
        return (Extracted { flags, grams: Vec::new() }, T::default());
    }
    greeg_lang::transcode_utf16(buf);
    let cf = content_flags(&buf[..buf.len().min(65536)], buf.len() as u64);
    flags.0 |= cf.0;
    if flags.has(FileFlags::BINARY | FileFlags::HUGE) {
        return (Extracted { flags, grams: Vec::new() }, T::default());
    }
    let t = hook(rel, buf);
    fold_buf(buf);
    dd.extract(buf, grams);
    (Extracted { flags, grams: std::mem::take(grams) }, t)
}

/// Build phase 1 into `dir`. Returns the manifest written.
pub fn build(root: &Path, dir: &Path, opts: &BuildOpts) -> Result<Manifest> {
    let t0 = Instant::now();
    fs::create_dir_all(dir)?;
    let fsevents_id = greeg_fsevents_id();
    let (walked, dirs) = walk(root)?;
    let walk_ms = t0.elapsed().as_secs_f64() * 1e3;

    let pool = rayon::ThreadPoolBuilder::new().num_threads(opts.reader_threads).build()?;
    let root_buf = root.to_path_buf();
    let source_bytes = std::sync::atomic::AtomicU64::new(0);
    type Flags = Vec<(u32, u16)>;
    type GramMap = HashMap<u32, Vec<u32>>;
    let (flags_by_id, merged): (Flags, GramMap) = pool.install(|| {
        // one accumulator per chunk (a few per thread), not per rayon split:
        // each carries a 2 MiB dedup bitset and a 32k-entry map
        let n_chunks = (opts.reader_threads * 4).max(1);
        let chunk = walked.len().div_ceil(n_chunks).max(1);
        let (flags, maps): (Flags, Vec<GramMap>) = walked
            .par_chunks(chunk)
            .enumerate()
            .map(|(ci, ws)| {
                let mut fl: Flags = Vec::with_capacity(ws.len());
                let mut map: GramMap = HashMap::with_capacity(1 << 15);
                let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
                let mut dd = Dedup::new();
                let mut grams: Vec<u32> = Vec::with_capacity(8192);
                for (j, w) in ws.iter().enumerate() {
                    let id = (ci * chunk + j) as u32;
                    let ex = extract_file(&root_buf.join(&w.rel), &w.rel, w.size, &mut buf, &mut dd, &mut grams);
                    if !ex.grams.is_empty() {
                        source_bytes.fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    for &g in &ex.grams {
                        map.entry(g).or_insert_with(|| Vec::with_capacity(8)).push(id);
                    }
                    grams = ex.grams;
                    fl.push((id, ex.flags.0));
                }
                (fl, vec![map])
            })
            .reduce(
                || (Vec::new(), Vec::new()),
                |(mut fa, mut ma), (mut fb, mut mb)| {
                    fa.append(&mut fb);
                    ma.append(&mut mb);
                    (fa, ma)
                },
            );
        // merge maps: largest first, absorb others
        let mut maps = maps;
        maps.sort_by_key(|m| std::cmp::Reverse(m.len()));
        let mut merged = maps.pop().unwrap_or_default();
        for m in maps {
            for (k, mut v) in m {
                merged.entry(k).or_default().append(&mut v);
            }
        }
        (flags, merged)
    });
    let extract_ms = t0.elapsed().as_secs_f64() * 1e3 - walk_ms;

    // postings
    let mut entries: Vec<(u32, Vec<u32>)> = merged.into_iter().collect();
    entries.par_sort_unstable_by_key(|(k, _)| *k);
    let entries: Vec<(u32, u32, Vec<u8>)> = entries
        .into_par_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            let bm = RoaringBitmap::from_sorted_iter(v.iter().copied()).unwrap();
            let mut out = Vec::with_capacity(bm.serialized_size());
            bm.serialize_into(&mut out).unwrap();
            (k, v.len() as u32, out)
        })
        .collect();

    // file table
    let mut ft = FileTable::default();
    let mut flags_sorted = flags_by_id;
    flags_sorted.sort_unstable_by_key(|(id, _)| *id);
    for (i, w) in walked.iter().enumerate() {
        let (off, len) = ft.intern(&w.rel);
        let flags = flags_sorted.get(i).map(|(_, f)| *f).unwrap_or(0);
        ft.push_file(&w.rel, FileRec { path_off: off, path_len: len, lang: Lang::from_path(Path::new(&w.rel)).code(), flags8: 0, size: w.size, mtime_ns: w.mtime_ns, dir: w.dir, flags, rank: 0 });
    }
    for d in &dirs {
        let (off, len) = ft.intern(&d.rel);
        ft.dirs.push(DirRec { path_off: off, path_len: len, pad: 0, mtime_ns: d.mtime_ns });
    }

    // publish under the writer lock: components, then the manifest, and only
    // then the previous generation and its deltas (readers that opened the old
    // manifest keep valid maps; readers that open the new one ignore delta/)
    let lock = crate::lock::writer(dir)?;
    let generation = read_manifest(dir).map(|m| m.generation + 1).unwrap_or(1);
    format::write_atomic(&dir.join(format!("grams.{generation}.bin")), format::COMP_GRAMS, &format::serialize_grams(&entries))?;
    format::write_atomic(&dir.join(format!("files.{generation}.bin")), format::COMP_FILES, &ft.serialize())?;
    let m = Manifest {
        format: crate::FORMAT_VERSION,
        root: root.to_string_lossy().into_owned(),
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
        fsevents_id,
        verified_unix_ms: now_ms(),
        deltas: 0,
        tombstones: 0,
    };
    write_manifest(dir, &m)?;
    // a fresh build supersedes deltas and older generations
    let _ = fs::remove_dir_all(dir.join("delta"));
    remove_stale(dir, &["grams.", "files."], generation);
    drop(lock);
    if !opts.quiet {
        eprintln!("greeg index: {} files, {} dirs, {:.1} MB source; walk {:.0} ms, extract {:.0} ms, total {:.0} ms; {} grams", walked.len(), dirs.len(), m.source_bytes as f64 / 1e6, walk_ms, extract_ms, m.build_ms, entries.len());
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
pub fn wants_symbols(rel: &str, flags: u16) -> bool {
    Lang::from_path(Path::new(rel)).has_grammar() && !FileFlags(flags).has(FileFlags::BINARY | FileFlags::HUGE | FileFlags::MINIFIED | FileFlags::LOCKFILE)
}

/// Read + extract one file for stage B.
pub fn extract_symbols(path: &Path, rel: &str, size: u64, buf: &mut Vec<u8>) -> Option<FileExtract> {
    if !read_file(path, size, buf) {
        return None;
    }
    greeg_lang::transcode_utf16(buf);
    if buf.len() as u64 > MAX_FILE || buf[..buf.len().min(8192)].contains(&0) {
        return None;
    }
    Some(symbols_of_bytes(rel, buf))
}

fn symbols_of_bytes(rel: &str, src: &[u8]) -> FileExtract {
    let lang = Lang::from_path(Path::new(rel));
    let ex = sym::extract(lang, sym::is_tsx(rel), src);
    FileExtract::from_extract(ex, src)
}

/// Phase 2 (DESIGN.md §4.1 stage B + merge): parse every file with a grammar
/// on all cores, resolve imports, run PageRank, publish symbols/spans/graph
/// and republish the file table with ranks and parse flags.
fn phase2(root: &Path, dir: &Path, ft: &mut FileTable, walked: &[WalkedFile], m: &mut Manifest, opts: &BuildOpts) -> Result<()> {
    let t0 = Instant::now();
    let generation = m.generation;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cores).build()?;
    let root_buf = root.to_path_buf();
    let todo: Vec<u32> = (0..ft.files.len()).filter(|&i| wants_symbols(&walked[i].rel, ft.files[i].flags)).map(|i| i as u32).collect();
    let extracts: Vec<(u32, Option<FileExtract>)> = pool.install(|| {
        sym::warm();
        todo.par_iter()
            .map_init(
                || Vec::<u8>::with_capacity(256 * 1024),
                |buf, &i| {
                    let w = &walked[i as usize];
                    (i, extract_symbols(&root_buf.join(&w.rel), &w.rel, w.size, buf))
                },
            )
            .collect()
    });
    let parse_ms = t0.elapsed().as_secs_f64() * 1e3;
    let mut by_file: Vec<Option<FileExtract>> = (0..ft.files.len()).map(|_| None).collect();
    let mut fallbacks = 0u32;
    for (i, ex) in extracts {
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
    // resolve imports → graph
    let rels: Vec<(u32, &str)> = walked.iter().enumerate().map(|(i, w)| (i as u32, w.rel.as_str())).collect();
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
        let lang = Lang::from_path(Path::new(&walked[i].rel));
        let ctx = resolver.file_ctx(lang, &walked[i].rel);
        let mut t = Vec::with_capacity(ex.imports.len());
        for im in &ex.imports {
            let ids = resolver.resolve_with(lang, &walked[i].rel, &ctx, im);
            t.push(ids.first().copied().unwrap_or(NONE));
            let w = (im.names.len().max(1) * 10).min(u16::MAX as usize) as u16;
            for id in ids {
                graph.add(i as u32, id, w);
            }
        }
        if lang == Lang::Kotlin && let Some(pkg) = &ex.package {
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
    // symbols + spans
    let mut sb = SymBuilder::new(n);
    let mut pb = SpanBuilder::new(n);
    for i in 0..ft.files.len() {
        sb.add_file(i as u32, by_file[i].as_ref());
        pb.add_file(i as u32, by_file[i].as_ref(), &targets[i]);
    }
    let n_symbols = sb.n_symbols() as u32;
    let sym_body = sb.finish(&|f| rank[f as usize]);
    let span_body = pb.finish();
    drop(resolver);
    drop(by_file);
    // publish under the lock; queries may have applied deltas against this
    // generation meanwhile, so the manifest keeps their count
    let lock = crate::lock::writer(dir)?;
    let Some(cur) = read_manifest(dir) else { bail!("manifest vanished during phase 2") };
    if cur.generation != generation {
        bail!("index generation {} superseded by {} during phase 2", generation, cur.generation);
    }
    format::write_atomic(&dir.join(format!("symbols.{generation}.bin")), format::COMP_SYMBOLS, &sym_body)?;
    format::write_atomic(&dir.join(format!("spans.{generation}.bin")), format::COMP_SPANS, &span_body)?;
    format::write_atomic(&dir.join(format!("graph.{generation}.bin")), format::COMP_GRAPH, &graph_body)?;
    format::write_atomic(&dir.join(format!("files.{generation}.bin")), format::COMP_FILES, &ft.serialize())?;
    m.deltas = cur.deltas;
    m.tombstones = cur.tombstones;
    m.verified_unix_ms = cur.verified_unix_ms;
    m.fsevents_id = cur.fsevents_id;
    m.phase2 = true;
    m.symbols = n_symbols;
    m.edges = n_edges as u32;
    m.parse_fallbacks = fallbacks;
    m.phase2_ms = t0.elapsed().as_secs_f64() * 1e3;
    m.build_ms += m.phase2_ms;
    write_manifest(dir, m)?;
    remove_stale(dir, &["symbols.", "spans.", "graph."], generation);
    drop(lock);
    if !opts.quiet {
        eprintln!("greeg index phase 2: {} files parsed on {} threads in {:.0} ms ({} regex fallbacks), resolve+rank {:.0} ms, {} symbols, {} edges, total {:.0} ms", todo.len(), cores, parse_ms, fallbacks, resolve_ms, n_symbols, n_edges, m.phase2_ms);
    }
    Ok(())
}

/// Delete component files of other generations (and leftover temp files).
fn remove_stale(dir: &Path, prefixes: &[&str], generation: u32) {
    let keep = format!(".{generation}.");
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stale_bin = prefixes.iter().any(|p| name.starts_with(p)) && name.ends_with(".bin") && !name.contains(&keep);
            let stale_tmp = name.ends_with(".tmp") && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs() > 600).unwrap_or(false);
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

/// Build a delta segment for `files` (absolute ids assigned by the caller).
/// Returns the serialized segment body.
pub fn build_delta(root: &Path, first_id: u32, files: &[WalkedFile], dirs: &[WalkedDir], tomb: &RoaringBitmap) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(256 * 1024);
    let mut dd = Dedup::new();
    let mut grams = Vec::with_capacity(8192);
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut ft = FileTable::default();
    let mut sb = SymBuilder::new(files.len() as u32);
    let mut pb = SpanBuilder::new(files.len() as u32);
    for (i, w) in files.iter().enumerate() {
        let id = first_id + i as u32;
        // one read serves both extractors: symbols see the unfolded bytes, then grams
        let (ex, fx) = extract_file_with(&root.join(&w.rel), &w.rel, w.size, &mut buf, &mut dd, &mut grams, |rel, src| {
            let flags = path_flags(rel).0 | content_flags(&src[..src.len().min(65536)], src.len() as u64).0;
            if wants_symbols(rel, flags) { Some(symbols_of_bytes(rel, src)) } else { None }
        });
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
        sb.add_file(i as u32, fx.as_ref());
        let no_targets: Vec<u32> = fx.as_ref().map(|f| vec![NONE; f.imports.len()]).unwrap_or_default();
        pb.add_file(i as u32, fx.as_ref(), &no_targets);
        let (off, len) = ft.intern(&w.rel);
        ft.push_file(&w.rel, FileRec { path_off: off, path_len: len, lang: Lang::from_path(Path::new(&w.rel)).code(), flags8: 0, size: w.size, mtime_ns: w.mtime_ns, dir: w.dir, flags, rank: 0 });
    }
    for d in dirs {
        let (off, len) = ft.intern(&d.rel);
        ft.dirs.push(DirRec { path_off: off, path_len: len, pad: 0, mtime_ns: d.mtime_ns });
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
    let sbb = sb.finish(&|_| 0.5);
    let pbb = pb.finish();
    let mut tb = Vec::new();
    tomb.serialize_into(&mut tb)?;
    let mut body = Vec::with_capacity(24 + fb.len() + gb.len() + sbb.len() + pbb.len() + tb.len());
    for x in [first_id, files.len() as u32, fb.len() as u32, gb.len() as u32, sbb.len() as u32, pbb.len() as u32] {
        body.extend_from_slice(&x.to_le_bytes());
    }
    body.extend_from_slice(&fb);
    body.extend_from_slice(&gb);
    body.extend_from_slice(&sbb);
    body.extend_from_slice(&pbb);
    body.extend_from_slice(&tb);
    Ok(body)
}

pub fn root_of(p: &Path) -> PathBuf {
    p.to_path_buf()
}
