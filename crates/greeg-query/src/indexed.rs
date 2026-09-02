//! Index-backed search: plan → candidates → verify only those files, then
//! classify hits from the stored span tables (DESIGN.md §6.2). Falls back to
//! scan mode (returns Ok(None)) when there is no usable index, spawning a
//! background build so the next query has one.

use crate::{Ctx, DefSummary, FileResult, Hit, HitKind, Mode, Options, Rung, ScanResult, Stats, is_word_byte, kind_by_context, process_file};
use anyhow::{Context, Result};
use greeg_index::format::SymRec;
use greeg_index::fresh::{self, Mode as Fresh};
use greeg_index::index::SymId;
use greeg_index::symtab::kind_from_code;
use greeg_index::{Index, index_dir_for, plan};
use greeg_lang::{DefKind, FileFlags, Lang};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

static PENDING_BUILD: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);

/// Queue a background build; it is started by `flush_pending_build` after the
/// answer is written, so the build's twelve threads never compete with the
/// scan that answers this query (measured: 0.2–0.4 s instead of 0.09 s on
/// django when the build started first).
pub fn spawn_build(root: &Path, dir: &Path) {
    if let Ok(mut g) = PENDING_BUILD.lock() {
        *g = Some((root.to_path_buf(), dir.to_path_buf()));
    }
}

/// Start the queued build, if any (call once, after output is flushed).
pub fn flush_pending_build() {
    let pending = PENDING_BUILD.lock().ok().and_then(|mut g| g.take());
    if let Some((root, dir)) = pending {
        spawn_build_now(&root, &dir);
    }
}

/// Spawn `greeg index --root ROOT` detached, unless a build is already running.
pub fn spawn_build_now(root: &Path, dir: &Path) {
    let _ = fs::create_dir_all(dir);
    let marker = dir.join("BUILDING");
    if let Ok(md) = fs::metadata(&marker) {
        if md.modified().ok().and_then(|m| m.elapsed().ok()).map(|e| e.as_secs() < 600).unwrap_or(true) {
            return;
        }
        let _ = fs::remove_file(&marker);
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(root_abs) = fs::canonicalize(root) else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index").arg("--root").arg(&root_abs).arg("--quiet");
    if let Some(d) = std::env::var_os("GREEG_INDEX_DIR") {
        cmd.env("GREEG_INDEX_DIR", d);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

/// Drop the manifest so the next query rebuilds, and start that rebuild now.
pub fn mark_corrupt(o: &Options) {
    let dir = match &o.index_dir {
        Some(d) => d.clone(),
        None => match index_dir_for(&o.root) {
            Ok(d) => d,
            Err(_) => return,
        },
    };
    let _ = fs::remove_file(dir.join("manifest"));
    let _ = fs::remove_dir_all(dir.join("delta"));
    spawn_build(&o.root, &dir);
}

fn path_allowed(rel: &str, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|p| p.is_empty() || p == "." || rel == p || rel.starts_with(&format!("{p}/")))
}

/// Result of opening the index for a query.
pub struct Opened {
    pub idx: Index,
    pub dir: PathBuf,
    pub fresh_method: &'static str,
    pub fresh_ms: f64,
    pub fresh_changed: usize,
}

/// Open the index for `o.root`, run the freshness check and apply deltas.
/// `Ok(None)` means no usable index (a background build was spawned when
/// possible) or that a rebuild is needed for this query.
pub fn open_fresh(o: &Options, threads: usize) -> Result<Option<Opened>> {
    let dir = match &o.index_dir {
        Some(d) => d.clone(),
        None => index_dir_for(&o.root)?,
    };
    let mut idx = match Index::open(&dir) {
        Ok(i) => i,
        Err(_) => {
            spawn_build(&o.root, &dir);
            return Ok(None);
        }
    };
    let t_fresh = Instant::now();
    let stat_threads = threads.clamp(1, 4);
    let mut fresh_method = if o.fresh == Fresh::None { "none" } else { "ttl" };
    let mut fresh_changed = 0;
    if let Some(ch) = fresh::check(&idx, &o.root, o.fresh, stat_threads) {
        fresh_method = ch.method;
        fresh_changed = ch.count() + ch.deleted.len();
        if fresh::needs_rebuild(&idx, &ch) {
            spawn_build(&o.root, &dir);
            return Ok(None);
        }
        if !ch.is_empty() {
            fresh::apply(&idx, &o.root, &ch).context("apply delta")?;
            idx = Index::open(&dir)?;
        } else {
            let _ = fresh::apply(&idx, &o.root, &ch); // refresh TTL and event id
        }
    }
    // a phase-1-only index answers gram queries; symbols arrive when the build finishes
    Ok(Some(Opened { idx, dir, fresh_method, fresh_ms: t_fresh.elapsed().as_secs_f64() * 1e3, fresh_changed }))
}

/// Try to answer from the index. Ok(None) means "use scan mode".
pub(crate) fn try_index(cx: &Ctx, threads: usize, t0: Instant) -> Result<Option<ScanResult>> {
    let o = cx.o;
    let Some(op) = open_fresh(o, threads)? else { return Ok(None) };
    let idx = &op.idx;
    // fault injection for the M5 robustness tests
    if std::env::var_os("GREEG_DEBUG_PANIC").is_some() {
        panic!("injected panic (GREEG_DEBUG_PANIC)");
    }
    #[cfg(unix)]
    if std::env::var_os("GREEG_DEBUG_SIGBUS").is_some() {
        unsafe { libc::raise(libc::SIGBUS) };
    }
    let mut stats = Stats { threads, source: "index", fresh_method: op.fresh_method, fresh_ms: op.fresh_ms, fresh_changed: op.fresh_changed, ..Default::default() };

    // plan
    let q = plan::plan(&o.pattern, o.fixed_strings, o.case_insensitive || (o.smart_case && !o.pattern.chars().any(|c| c.is_uppercase())))?;
    stats.plan = format!("{q:?}");
    let cands = idx.candidates(&q);
    stats.files_walked = idx.live().len() as usize;

    // filters: paths, globs, types, flag exclusions
    let paths: Vec<String> = o.paths.iter().map(|p| p.to_string_lossy().trim_start_matches("./").trim_end_matches('/').to_string()).collect();
    let overrides = if o.globs.is_empty() {
        None
    } else {
        let mut ob = ignore::overrides::OverrideBuilder::new(&o.root);
        for g in &o.globs {
            ob.add(g)?;
        }
        Some(ob.build()?)
    };
    let types = if o.types.is_empty() && o.types_not.is_empty() { None } else { Some(crate::build_types(o)?) };
    let mut entries: Vec<(u8, u32, String)> = Vec::with_capacity(cands.len() as usize);
    'files: for id in cands.iter() {
        let Some(rec) = idx.rec(id) else { continue };
        let rel = idx.path(id).unwrap_or("");
        if !path_allowed(rel, &paths) {
            continue;
        }
        // ripgrep precedence: an override glob decides first (whitelist wins over
        // types); ignore globs also apply to every ancestor directory, as the
        // walker would prune them.
        let mut decided = false;
        if let Some(ov) = &overrides {
            let m = ov.matched(rel, false);
            if m.is_ignore() {
                continue;
            }
            decided = m.is_whitelist();
            let mut end = 0;
            while let Some(k) = rel[end..].find('/') {
                end += k;
                if ov.matched(&rel[..end], true).is_ignore() {
                    continue 'files;
                }
                end += 1;
            }
        }
        if !decided
            && let Some(t) = &types
            && t.matched(rel, false).is_ignore()
        {
            continue;
        }
        let flags = FileFlags(rec.flags);
        if flags.has(FileFlags::BINARY) {
            continue;
        }
        if !o.all && (o.no_tests && flags.has(FileFlags::TEST) || o.no_vendored && flags.has(FileFlags::VENDORED) || o.no_generated && flags.has(FileFlags::GENERATED | FileFlags::MINIFIED | FileFlags::LOCKFILE)) {
            continue;
        }
        // prior order: source before demoted, then path
        let prio = if flags.has(FileFlags::MINIFIED) { 3 } else if flags.demoted() { 2 } else { 0 };
        entries.push((prio, id, rel.to_string()));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
    stats.candidates = entries.len();

    // verify candidates with the reader pool; classify from the index when it has symbols
    let use_spans = idx.has_symbols() && cx.classify;
    let cx2 = Ctx { o, matcher: cx.matcher, stats: cx.stats, classify: cx.classify && !use_spans, filter_kinds: !use_spans };
    let cx = &cx2;
    let out: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let root = &o.root;
    let n_threads = threads.clamp(1, 8).min(entries.len().max(1));
    std::thread::scope(|sc| {
        for _ in 0..n_threads {
            sc.spawn(|| {
                let mut sb = SearcherBuilder::new();
                sb.line_number(true).binary_detection(BinaryDetection::quit(0)).multi_line(o.multiline).bom_sniffing(false);
                let mut searcher = sb.build();
                let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
                let mut local: Vec<FileResult> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= entries.len() {
                        break;
                    }
                    let (_, id, rel) = &entries[i];
                    let path: PathBuf = root.join(rel);
                    if let Some(mut fr) = process_file(cx, &path, rel.clone(), &mut searcher, &mut buf) {
                        fr.file_id = Some(*id);
                        let t = Instant::now();
                        if use_spans {
                            classify_from_index(idx, *id, &mut fr, o, &buf);
                        }
                        // PageRank term of the prior (DESIGN.md §6.3): 0.8 was the placeholder
                        let rank = idx.rank(*id);
                        let k = (0.6 + 0.4 * rank) / 0.8;
                        fr.prior *= k;
                        for h in &mut fr.hits {
                            h.score *= k;
                        }
                        cx.stats.classify_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                        if !fr.hits.is_empty() {
                            local.push(fr);
                        }
                    }
                }
                if !local.is_empty() {
                    out.lock().unwrap().append(&mut local);
                }
            });
        }
    });
    let mut files = out.into_inner().unwrap();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    crate::finish_stats(&mut stats, &files, o, cx.stats, t0);
    if idx.corrupt() {
        // a posting list failed to deserialize: the query above used a superset
        // (every file) for that gram, so the answer is right; rebuild in the background
        eprintln!("greeg: index component unreadable; rebuilding in the background");
        mark_corrupt(o);
    }
    let _ = Mode::Files;
    Ok(Some(ScanResult { opts: o.clone(), files, stats, rung: Rung::Exact, ignored_only: None }))
}

fn chain_of(idx: &Index, s: SymId) -> Vec<(DefKind, String)> {
    idx.sym_chain(s).into_iter().map(|(k, n)| (kind_from_code(k), n.to_string())).collect()
}

/// Definition summaries of a file from the index.
pub fn defs_of(idx: &Index, id: u32) -> Vec<DefSummary> {
    let Some((first, syms)) = idx.symbols_of(id) else { return Vec::new() };
    syms.iter()
        .enumerate()
        .map(|(i, s)| {
            let sid = SymId { seg: first.seg, idx: first.idx + i as u32 };
            DefSummary { name: idx.sym_name(sid).to_string(), kind: kind_from_code(s.kind), line: s.line, start: s.start, end: s.end, chain: chain_of(idx, sid), flags: s.flags }
        })
        .collect()
}

/// Classify every hit of `f` (index file `id`) from the span tables, attach
/// chains and definitions, apply `--kind`, and rescore.
pub(crate) fn classify_from_index(idx: &Index, id: u32, f: &mut FileResult, o: &Options, src: &[u8]) {
    let Some((first, syms)) = idx.symbols_of(id) else { return };
    let pat = o.pattern.as_bytes();
    let mut kinds = [0u32; 9];
    let mut hits: Vec<Hit> = Vec::with_capacity(f.hits.len());
    for mut h in f.hits.drain(..) {
        let (ms, me) = (h.match_start, h.match_end);
        let (kind, di) = classify_hit(idx, id, first, syms, f.lang, src, &h);
        if !o.kinds.is_empty() && !o.kinds.contains(&kind) {
            continue;
        }
        h.kind = kind;
        h.def_idx = di;
        h.chain = di.map(|d| chain_of(idx, SymId { seg: first.seg, idx: first.idx + d })).unwrap_or_default();
        let exact = !o.fixed_strings && !o.case_insensitive && src.get(ms as usize..me as usize) == Some(pat) && (ms == 0 || !is_word_byte(src[ms as usize - 1])) && (me as usize >= src.len() || !is_word_byte(src[me as usize]));
        h.score = kind.weight() * f.prior * if exact { 1.15 } else { 1.0 };
        kinds[kind.idx()] += 1;
        hits.push(h);
    }
    f.hits = hits;
    f.kinds = kinds;
    // Only the definitions the hits point at are materialized (a file can hold
    // thousands of symbols; building every summary cost ~100 µs per file).
    let mut used: Vec<u32> = f.hits.iter().filter_map(|h| h.def_idx).collect();
    used.sort_unstable();
    used.dedup();
    let mut defs: Vec<DefSummary> = Vec::with_capacity(used.len());
    for &d in &used {
        let s = &syms[d as usize];
        let sid = SymId { seg: first.seg, idx: first.idx + d };
        defs.push(DefSummary { name: idx.sym_name(sid).to_string(), kind: kind_from_code(s.kind), line: s.line, start: s.start, end: s.end, chain: Vec::new(), flags: s.flags });
    }
    for h in &mut f.hits {
        if let Some(d) = h.def_idx {
            h.def_idx = used.binary_search(&d).ok().map(|i| i as u32);
        }
    }
    f.defs = defs;
    f.refined = true;
}

/// Kind and local definition index for one hit (DESIGN.md §6.2 steps 1–4).
fn classify_hit(idx: &Index, id: u32, first: SymId, syms: &[SymRec], lang: Lang, src: &[u8], h: &Hit) -> (HitKind, Option<u32>) {
    let (ms, me) = (h.match_start, h.match_end);
    if !lang.has_grammar() {
        return (HitKind::Ident, None);
    }
    let enclosing = || -> Option<u32> {
        let upto = syms.partition_point(|s| s.start <= ms);
        (0..upto).rev().find(|&i| syms[i].end > ms).map(|i| i as u32)
    };
    if let Some((k, _, _)) = idx.noncode_at(id, ms) {
        let kind = match k {
            0 => HitKind::Comment,
            1 => HitKind::Str,
            _ => HitKind::Docstring,
        };
        return (kind, enclosing());
    }
    // definition name overlap: symbols are sorted by start; a name is within 512 bytes of its start
    let upto = syms.partition_point(|s| s.start < me);
    for i in (0..upto).rev() {
        let s = &syms[i];
        let ne = s.name_start + s.name_len as u32;
        if s.name_start < me && ms < ne {
            return (HitKind::Def, Some(i as u32));
        }
        if s.start + 512 < ms {
            break;
        }
    }
    if idx.import_at(id, ms).is_some() {
        return (HitKind::Import, enclosing());
    }
    let ls = h.line_start as usize;
    let le = memchr::memchr(b'\n', &src[ls..]).map(|k| ls + k).unwrap_or(src.len());
    let _ = first;
    (kind_by_context(lang, src, ms as usize, me as usize, ls, &src[ls..le]), enclosing())
}
