//! Index-backed search: plan → candidates → verify only those files, then
//! classify hits from the stored span tables. Falls back to
//! scan mode (returns Ok(None)) when there is no usable index, spawning a
//! background build so the next query has one.

use crate::{
    Ctx, DefSummary, FileResult, Hit, HitKind, Mode, Options, Rung, ScanResult, Stats,
    kind_by_context, process_file,
};
use anyhow::{Context, Result};
use greeg_index::format::{NONE, SymRec};
use greeg_index::fresh::{self, Mode as Fresh};
use greeg_index::index::SymId;
use greeg_index::symtab::kind_from_code;
use greeg_index::{Index, RootId, lock, plan, read_manifest};
use greeg_lang::sym::SYM_OBJ_MEMBER;
use greeg_lang::{DefKind, FileFlags, Lang};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

/// Queued background work: (root, index dir, refresh). `refresh` is the
/// detached delta publish of an answer-first query; a full build wins when
/// both are queued.
static PENDING_BUILD: Mutex<Option<(PathBuf, PathBuf, bool)>> = Mutex::new(None);

/// Queue a background build; it is started by `flush_pending_build` after the
/// answer is written, so the build's twelve threads never compete with the
/// scan that answers this query (measured: 0.2–0.4 s instead of 0.09 s on
/// django when the build started first).
pub fn spawn_build(root: &Path, dir: &Path) {
    if let Ok(mut g) = PENDING_BUILD.lock() {
        *g = Some((root.to_path_buf(), dir.to_path_buf(), false));
    }
}

/// Queue a detached `greeg index --refresh`: this query
/// answered around the changed files itself, and the delta is published after
/// the output so an edit never delays the search that follows it.
pub fn spawn_refresh(root: &Path, dir: &Path) {
    if let Ok(mut g) = PENDING_BUILD.lock()
        && !matches!(&*g, Some((_, _, false)))
    {
        *g = Some((root.to_path_buf(), dir.to_path_buf(), true));
    }
}

/// Start the queued build or refresh, if any (call once, after output is flushed).
pub fn flush_pending_build() {
    let pending = PENDING_BUILD.lock().ok().and_then(|mut g| g.take());
    if let Some((root, dir, refresh)) = pending {
        spawn_build_now(&root, &dir, refresh);
    }
}

fn marker_younger_than(marker: &Path, secs: u64) -> bool {
    fs::metadata(marker)
        .ok()
        .map(|md| {
            md.modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|e| e.as_secs() < secs)
                .unwrap_or(true)
        })
        .unwrap_or(false)
}

/// Spawn `greeg index --root ROOT [--refresh]` detached, unless the same work
/// is already running: a `BUILDING` marker younger than ten minutes, or for a
/// refresh a `REFRESHING` marker younger than thirty seconds (a running full
/// build also makes a refresh pointless).
pub fn spawn_build_now(root: &Path, dir: &Path, refresh: bool) {
    let _ = greeg_index::create_private_dir(dir);
    let building = dir.join("BUILDING");
    if marker_younger_than(&building, 600) {
        return;
    }
    if refresh {
        if marker_younger_than(&dir.join("REFRESHING"), 30) {
            return;
        }
    } else if building.exists() {
        let _ = fs::remove_file(&building);
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(root_abs) = fs::canonicalize(root) else {
        return;
    };
    // The markers above live in `dir`; the work must publish there too
    // (`--index-dir` names the repository directory that holds it).
    let Ok(dir_abs) = std::path::absolute(greeg_index::repo_of(dir)) else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index")
        .arg("--root")
        .arg(&root_abs)
        .arg("--index-dir")
        .arg(&dir_abs)
        .arg("--quiet");
    if refresh {
        cmd.arg("--refresh");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

/// The detached step of an answer-first query (`greeg index --refresh`): run
/// the freshness check without the TTL and publish the delta, or queue a full
/// build when the change set is past the inline threshold. One refresher at
/// a time per index; a marker older than thirty seconds is taken over.
pub fn refresh_now(root: &Path, dir: &Path, threads: usize) -> Result<()> {
    let marker = dir.join("REFRESHING");
    match greeg_index::private_file()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            let _ = greeg_index::owner_only(&f);
            let _ = writeln!(f, "{}", std::process::id());
        }
        Err(_) => {
            if marker_younger_than(&marker, 30) {
                return Ok(());
            }
            // a stale marker (its refresher died): take it over, and renew it so
            // the next spawn sees a live refresher instead of joining in
            let _ = fs::write(&marker, format!("{}\n", std::process::id()));
        }
    }
    let r = refresh_inner(root, dir, threads);
    let _ = fs::remove_file(&marker);
    r
}

fn refresh_inner(root: &Path, dir: &Path, threads: usize) -> Result<()> {
    let Ok(idx) = Index::open(dir) else {
        return Ok(()); // no index: the query queued a build
    };
    if !built_for(&idx, root) {
        return Ok(());
    }
    let mode = fresh::explicit_mode(&idx);
    let Some(ch) = fresh::check(&idx, root, mode, threads.clamp(1, 4)) else {
        return Ok(());
    };
    if fresh::needs_rebuild(&idx, &ch) {
        drop(idx);
        spawn_build_now(root, dir, false);
        return Ok(());
    }
    fresh::apply(&idx, root, &ch).context("apply delta")?;
    Ok(())
}

/// Whether `idx` was built for `root`: a directory shared by two roots
/// (`--index-dir`) never answers for the other one.
fn built_for(idx: &Index, root: &Path) -> bool {
    RootId::of(root).is_some_and(|r| r == idx.manifest.root_id)
}

/// Generation + 1 of the index this process last opened through `open_fresh`
/// (0 = none), so `mark_corrupt` can tell the manifest that failed from one a
/// rebuild published since.
static OPENED_GEN: AtomicU64 = AtomicU64::new(0);

/// Drop the manifest so the next query rebuilds, and start that rebuild now.
/// Runs under the writer lock, and only while the manifest is still the one
/// this process opened: a generation a concurrent rebuild published since the
/// failure is left alone (a process that never opened an index deletes
/// unconditionally, e.g. after a SIGBUS re-exec).
pub fn mark_corrupt(o: &Options) {
    let Ok(dir) = greeg_index::index_dir(&o.root, o.index_dir.as_deref()) else {
        return;
    };
    let Ok(_lock) = lock::writer(&dir) else {
        return;
    };
    let opened = OPENED_GEN.load(Relaxed);
    if opened != 0
        && let Some(m) = read_manifest(&dir)
        && u64::from(m.generation) + 1 != opened
    {
        return;
    }
    let _ = fs::remove_file(dir.join("manifest"));
    let _ = fs::remove_dir_all(dir.join("delta"));
    spawn_build(&o.root, &dir);
}

pub(crate) fn path_allowed(rel: &str, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|p| {
        p.is_empty()
            || p == "."
            || rel == p
            || (rel.len() > p.len()
                && rel.starts_with(p.as_str())
                && rel.as_bytes()[p.len()] == b'/')
    })
}

/// A positional path as a root-relative path (C1). Plain relative paths are
/// normalized textually; absolute paths and paths with `..` are canonicalized
/// against the root. `None` means the path is outside the root (or cannot be
/// resolved): the caller answers that query from a scan.
pub fn rel_of(root: &Path, p: &Path) -> Option<String> {
    use std::path::Component;
    let simple = p.is_relative()
        && !p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if simple {
        let parts: Vec<String> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        return Some(parts.join("/"));
    }
    let root_c = fs::canonicalize(root).ok()?;
    let pc = fs::canonicalize(p).ok()?;
    let rel = pc.strip_prefix(&root_c).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Result of opening the index for a query.
pub struct Opened {
    pub idx: Index,
    pub dir: PathBuf,
    pub fresh_method: &'static str,
    pub fresh_ms: f64,
    pub fresh_changed: usize,
    /// Changes found but not applied (`open_fresh_deferred`): the caller
    /// searches these files itself and a detached refresh publishes the delta.
    pub pending: Option<fresh::Changes>,
}

/// Open the index for `o.root`, run the freshness check and apply deltas.
/// `Ok(None)` means no usable index (a background build was spawned when
/// possible) or that a rebuild is needed for this query.
pub fn open_fresh(o: &Options, threads: usize) -> Result<Option<Opened>> {
    open_fresh_with(o, threads, false, &mut || {})
}

/// `open_fresh` for a search (answer first): changes below
/// the rebuild threshold are returned in `pending` instead of being applied,
/// and a detached `greeg index --refresh` is queued for after the output.
/// Verbs keep `open_fresh`: they need the symbols of the changed files.
pub fn open_fresh_deferred(o: &Options, threads: usize) -> Result<Option<Opened>> {
    open_fresh_with(o, threads, true, &mut || {})
}

/// The check to run again after another writer republished: the same mode,
/// except that `Auto` must not trust the TTL stamp the other writer just wrote
/// (its walk may predate the change this query saw).
fn retry_mode(idx: &Index, mode: Fresh) -> Fresh {
    match mode {
        Fresh::Auto => fresh::explicit_mode(idx),
        m => m,
    }
}

/// `open_fresh` with a hook run between the freshness check and the delta
/// publish: a no-op in production, a racing writer in tests.
fn open_fresh_with(
    o: &Options,
    threads: usize,
    defer: bool,
    before_apply: &mut dyn FnMut(),
) -> Result<Option<Opened>> {
    let dir = greeg_index::index_dir(&o.root, o.index_dir.as_deref())?;
    let mut idx = match Index::open(&dir) {
        Ok(i) if built_for(&i, &o.root) => i,
        _ => {
            spawn_build(&o.root, &dir);
            return Ok(None);
        }
    };
    let t_fresh = Instant::now();
    let stat_threads = threads.clamp(1, 4);
    let mut fresh_method = if o.fresh == Fresh::None {
        "none"
    } else {
        "ttl"
    };
    let mut fresh_changed = 0;
    let mut pending = None;
    let mut mode = o.fresh;
    // A delta is published only against the manifest it was computed from
    // (`fresh::apply` returns 0 when a build or another query republished in
    // between). The reopened index may then lack the change this query saw,
    // so check it once more; bounded so two writers cannot ping-pong.
    for attempt in 0..2 {
        let Some(ch) = fresh::check(&idx, &o.root, mode, stat_threads) else {
            break;
        };
        fresh_method = ch.method;
        fresh_changed = ch.count() + ch.deleted.len();
        if fresh::needs_rebuild(&idx, &ch) {
            spawn_build(&o.root, &dir);
            return Ok(None);
        }
        if ch.is_empty() {
            let _ = fresh::apply(&idx, &o.root, &ch); // refresh TTL and event id
            break;
        }
        if defer {
            spawn_refresh(&o.root, &dir);
            pending = Some(ch);
            break;
        }
        before_apply();
        let applied = fresh::apply(&idx, &o.root, &ch).context("apply delta")?;
        idx = Index::open(&dir)?;
        if applied > 0 || attempt > 0 {
            break;
        }
        mode = retry_mode(&idx, mode);
    }
    OPENED_GEN.store(u64::from(idx.manifest.generation) + 1, Relaxed);
    // a phase-1-only index answers gram queries; symbols arrive when the build finishes
    Ok(Some(Opened {
        idx,
        dir,
        fresh_method,
        fresh_ms: t_fresh.elapsed().as_secs_f64() * 1e3,
        fresh_changed,
        pending,
    }))
}

/// Try to answer from the index. Ok(None) means "use scan mode".
pub(crate) fn try_index(cx: &Ctx, threads: usize, t0: Instant) -> Result<Option<ScanResult>> {
    let o = cx.o;
    let Some(op) = open_fresh_deferred(o, threads)? else {
        return Ok(None);
    };
    let idx = &op.idx;
    // fault injection for the M5 robustness tests
    if std::env::var_os("GREEG_DEBUG_PANIC").is_some() {
        panic!("injected panic (GREEG_DEBUG_PANIC)");
    }
    #[cfg(unix)]
    if std::env::var_os("GREEG_DEBUG_SIGBUS").is_some() {
        unsafe { libc::raise(libc::SIGBUS) };
    }
    let mut stats = Stats {
        threads,
        source: "index",
        fresh_method: op.fresh_method,
        fresh_ms: op.fresh_ms,
        fresh_changed: op.fresh_changed,
        ..Default::default()
    };

    // plan: whole-word queries open only the files that hold the word (the
    // word postings); everything else takes the trigram plan
    let casei =
        o.case_insensitive || (o.smart_case && !o.pattern.chars().any(|c| c.is_uppercase()));
    let q = plan::plan(&o.pattern, o.fixed_strings, casei)?;
    let identifier = o.budget != 0
        && !matches!(o.mode, Mode::Files | Mode::Count)
        && crate::shape::identifier_query(o);
    let mut wq = plan::word_plan(&o.pattern, o.fixed_strings, casei, o.word, identifier);
    // a bare identifier no live file holds as a whole word: its near-misses
    // (`zeta` inside `zeta_new`) are this rung's answer, as before the word
    // postings, so the trigram plan opens them instead of the ladder climbing
    // to a mislabelled case-insensitive rung
    if identifier
        && !o.word
        && wq
            .as_deref()
            .is_some_and(|alts| idx.word_candidates(alts).is_empty())
    {
        wq = None;
    }
    stats.plan = match &wq {
        Some(alts) => format!(
            "words {:?} · {q:?}",
            alts.iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect::<Vec<_>>()
        ),
        None => format!("{q:?}"),
    };
    let mut cands = idx.candidates_with(&q, wq.as_deref());
    let related_index = if identifier && wq.is_some() {
        idx.words_containing(&o.pattern, 4)
    } else {
        Vec::new()
    };
    stats.files_walked = idx.live_count() as usize;
    // answer first: files the freshness check found changed
    // are searched from disk below with scan-mode classification, their
    // indexed versions leave the candidates, and the delta is published after
    // the output by the detached refresh `open_fresh_deferred` queued
    let mut extras: Vec<(String, u32)> = Vec::new(); // (rel, superseded id or NONE)
    if let Some(ch) = &op.pending {
        for (id, w) in &ch.modified {
            cands.remove(*id);
            extras.push((w.rel.clone(), *id));
        }
        for id in &ch.deleted {
            cands.remove(*id);
        }
        for w in &ch.added {
            extras.push((w.rel.clone(), NONE));
        }
        stats.fresh_deferred = extras.len() + ch.deleted.len();
    }

    // filters: paths, globs, types, flag exclusions
    let mut paths: Vec<String> = Vec::with_capacity(o.paths.len());
    // ripgrep prints paths as given: an absolute (or `..`) positional path is
    // shown as that prefix plus the remainder, not root-relative
    let mut display: Vec<Option<String>> = Vec::with_capacity(o.paths.len());
    for p in &o.paths {
        match rel_of(&o.root, p) {
            Some(rel) => {
                // ripgrep searches a file named on the command line whatever
                // the ignore rules say; the index only holds walked files, so
                // an ignored or hidden one is answered by scan mode
                if p.is_file()
                    && !extras.iter().any(|(r, _)| *r == rel)
                    && !idx.live_files().any(|(_, r, _)| r == rel)
                {
                    return Ok(None);
                }
                // likewise a hidden or ignored directory: the walk enters it
                if p.is_dir()
                    && !rel.is_empty()
                    && !idx.has_dir(&rel)
                    && !op
                        .pending
                        .as_ref()
                        .is_some_and(|ch| ch.added_dirs.iter().any(|d| d.rel == rel))
                {
                    return Ok(None);
                }
                let given = p.to_string_lossy();
                let given = given.trim_end_matches('/');
                display.push(
                    if given.trim_start_matches("./") == rel
                        || rel.is_empty() && matches!(given, "." | "")
                    {
                        None
                    } else {
                        Some(given.to_string())
                    },
                );
                paths.push(rel);
            }
            None => return Ok(None), // outside the index root: scan mode answers this query
        }
    }
    let sel = crate::select::Selection::new(o, paths)?;
    // files the index skipped that this request selects are read from disk
    // like changed ones; anything else it selects needs the scan
    let Some(also) = sel.coverage(idx, op.pending.as_ref()) else {
        return Ok(None);
    };
    extras.extend(also.into_iter().map(|rel| (rel, NONE)));
    let display_rel = |rel: &str| -> Option<String> {
        let paths = sel.paths();
        let i = paths
            .iter()
            .position(|p| path_allowed(rel, std::slice::from_ref(p)))?;
        let d = display[i].as_ref()?;
        Some(if rel == paths[i] {
            d.clone()
        } else if paths[i].is_empty() {
            format!("{d}/{rel}")
        } else {
            format!("{d}/{}", &rel[paths[i].len() + 1..])
        })
    };
    // (prio, id or NONE for a changed file read from disk, superseded id, rel)
    let mut entries: Vec<(u8, u32, u32, &str)> =
        Vec::with_capacity(cands.len() as usize + extras.len());
    for id in cands.iter() {
        let Some(rec) = idx.rec(id) else { continue };
        let rel = idx.path(id).unwrap_or("");
        if let Some(prio) = sel.admit(rel, FileFlags(rec.flags)) {
            entries.push((prio, id, NONE, rel));
        }
    }
    for (rel, prev) in &extras {
        if let Some(prio) = sel.admit(rel, greeg_lang::path_flags(rel)) {
            entries.push((prio, NONE, *prev, rel.as_str()));
        }
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(b.3)));
    stats.candidates = entries.len();

    // verify candidates with the reader pool; classify from the index when it has symbols
    let use_spans = idx.has_symbols() && cx.classify;
    let cx_idx = Ctx {
        o,
        matcher: cx.matcher,
        stats: cx.stats,
        classify: cx.classify && !use_spans,
        filter_kinds: !use_spans,
        regular_only: true,
    };
    // a changed file has no spans yet: line-local classification, as in a scan
    let cx_scan = Ctx {
        o,
        matcher: cx.matcher,
        stats: cx.stats,
        classify: cx.classify,
        filter_kinds: true,
        regular_only: true,
    };
    let cx = &cx_idx;
    let cx_scan = &cx_scan;
    let out: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let root = &o.root;
    let n_threads = threads.clamp(1, 8).min(entries.len().max(1));
    std::thread::scope(|sc| {
        for _ in 0..n_threads {
            sc.spawn(|| {
                let mut sb = SearcherBuilder::new();
                sb.line_number(true)
                    .binary_detection(BinaryDetection::quit(0))
                    .multi_line(o.multiline)
                    .bom_sniffing(false);
                let mut searcher = sb.build();
                let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
                let mut local: Vec<FileResult> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= entries.len() {
                        break;
                    }
                    let (_, id, prev, rel) = &entries[i];
                    let changed = *id == NONE;
                    let path: PathBuf = root.join(rel);
                    // an indexed file classifies from its span tables; one
                    // without them, like a changed file, by the scan rules
                    let spans = if use_spans && !changed {
                        idx.symbols_of(*id)
                    } else {
                        None
                    };
                    let lang = Lang::from_path(&path);
                    let fid = *id;
                    let by_index = spans.map(|(_, syms)| {
                        move |src: &[u8], ms: u32, me: u32, ls: u32| {
                            classify_at(idx, fid, syms, lang, src, ms, me, ls).0
                        }
                    });
                    if let Some(mut fr) = process_file(
                        if changed || use_spans && spans.is_none() {
                            cx_scan
                        } else {
                            cx
                        },
                        &path,
                        rel.to_string(),
                        &mut searcher,
                        &mut buf,
                        by_index.as_ref().map(|f| f as crate::SpanKindOf),
                    ) {
                        if !changed {
                            fr.file_id = Some(*id);
                        }
                        if let Some(d) = display_rel(rel) {
                            fr.rel = d;
                        }
                        let t = Instant::now();
                        if spans.is_some() {
                            classify_from_index(idx, *id, &mut fr, o, &buf);
                        }
                        // PageRank term of the prior: 0.8 was the
                        // placeholder; an edited file keeps its rank, a new one is neutral
                        let rank = if !changed {
                            idx.rank(*id)
                        } else if *prev != NONE {
                            idx.rank(*prev)
                        } else {
                            0.5
                        };
                        let k = (0.6 + 0.4 * rank) / 0.8;
                        fr.prior *= k;
                        for h in &mut fr.hits {
                            h.score *= k;
                        }
                        cx.stats
                            .classify_ns
                            .fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
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
    Ok(Some(ScanResult {
        opts: o.clone(),
        files,
        stats,
        rung: Rung::Exact,
        ignored_only: None,
        ignored_partial: false,
        related_index,
    }))
}

fn chain_of(idx: &Index, s: SymId) -> Vec<(DefKind, String)> {
    idx.sym_chain(s)
        .into_iter()
        .map(|(k, n)| (kind_from_code(k), n.to_string()))
        .collect()
}

/// Definition summaries of a file from the index.
pub fn defs_of(idx: &Index, id: u32) -> Vec<DefSummary> {
    let Some((first, syms)) = idx.symbols_of(id) else {
        return Vec::new();
    };
    syms.iter()
        .enumerate()
        .map(|(i, s)| {
            let sid = SymId {
                seg: first.seg,
                idx: first.idx + i as u32,
            };
            DefSummary {
                name: idx.sym_name(sid).to_string(),
                kind: kind_from_code(s.kind),
                line: s.line,
                start: s.start,
                end: s.end,
                chain: chain_of(idx, sid),
                flags: s.flags,
            }
        })
        .collect()
}

/// Classify every hit of `f` (index file `id`) from the span tables, attach
/// chains and definitions, apply `--kind`, and rescore.
pub(crate) fn classify_from_index(
    idx: &Index,
    id: u32,
    f: &mut FileResult,
    o: &Options,
    src: &[u8],
) {
    let Some((first, syms)) = idx.symbols_of(id) else {
        return;
    };
    let mut kinds = [0u32; 9];
    let mut hits: Vec<Hit> = Vec::with_capacity(f.hits.len());
    for mut h in f.hits.drain(..) {
        let (kind, di) = classify_at(
            idx,
            id,
            syms,
            f.lang,
            src,
            h.match_start,
            h.match_end,
            h.line_start,
        );
        if !o.kinds.is_empty() && !o.kinds.contains(&kind) {
            continue;
        }
        h.kind = kind;
        h.def_idx = di;
        h.chain = di
            .map(|d| {
                chain_of(
                    idx,
                    SymId {
                        seg: first.seg,
                        idx: first.idx + d,
                    },
                )
            })
            .unwrap_or_default();
        h.score = kind.weight() * f.prior * crate::exact_boost(kind, h.exact);
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
        let sid = SymId {
            seg: first.seg,
            idx: first.idx + d,
        };
        defs.push(DefSummary {
            name: idx.sym_name(sid).to_string(),
            kind: kind_from_code(s.kind),
            line: s.line,
            start: s.start,
            end: s.end,
            chain: Vec::new(),
            flags: s.flags,
        });
    }
    for h in &mut f.hits {
        if let Some(d) = h.def_idx {
            h.def_idx = used.binary_search(&d).ok().map(|i| i as u32);
        }
    }
    f.defs = defs;
    f.refined = true;
}

/// Kind and local definition index for one occurrence, from the span tables.
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify_at(
    idx: &Index,
    id: u32,
    syms: &[SymRec],
    lang: Lang,
    src: &[u8],
    ms: u32,
    me: u32,
    line_start: u32,
) -> (HitKind, Option<u32>) {
    if !lang.has_grammar() {
        return (HitKind::Ident, None);
    }
    let enclosing = || -> Option<u32> {
        let upto = syms.partition_point(|s| s.start <= ms);
        (0..upto)
            .rev()
            .find(|&i| syms[i].end > ms)
            .map(|i| i as u32)
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
            // an object-literal member implements a typed member: `member`, not `def`
            if s.flags & SYM_OBJ_MEMBER != 0 {
                return (HitKind::Member, Some(i as u32));
            }
            return (HitKind::Def, Some(i as u32));
        }
        if s.start + 512 < ms {
            break;
        }
    }
    if idx.import_at(id, ms).is_some() {
        return (HitKind::Import, enclosing());
    }
    let ls = line_start as usize;
    let le = memchr::memchr(b'\n', &src[ls..])
        .map(|k| ls + k)
        .unwrap_or(src.len());
    (
        kind_by_context(
            lang,
            src,
            ms as usize,
            (me as usize).min(le),
            ls,
            &src[ls..le],
        ),
        enclosing(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query whose delta was skipped because another writer republished in
    /// between must re-check the reopened index instead of answering from it.
    #[test]
    fn open_fresh_rechecks_when_apply_skipped() {
        use greeg_index::build::{BuildOpts, build};
        let base = std::env::temp_dir().join(format!("greeg-open-fresh-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { helper_alpha(); }\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn helper_alpha() {}\n").unwrap();
        // enough files that two edits stay below the inline-apply threshold (`needs_rebuild`)
        for i in 0..60 {
            fs::write(
                root.join(format!("src/f{i}.rs")),
                format!("pub fn filler_{i}() {{}}\n"),
            )
            .unwrap();
        }
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
        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: Fresh::Stat,
            ..Default::default()
        };

        // another query opened the index, then saw only the first edit
        let other = Index::open(&dir).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { helper_alpha(); omega_one(); }\n",
        )
        .unwrap();
        let other_ch = fresh::check(&other, &root, Fresh::Stat, 1).unwrap();
        assert_eq!(other_ch.modified.len(), 1);
        fs::write(
            root.join("src/lib.rs"),
            "pub fn helper_alpha() {}\npub fn omega_two() {}\n",
        )
        .unwrap();

        // this query sees both edits, but the other one publishes first
        let mut raced = false;
        let mut race = || {
            if !raced {
                raced = true;
                assert_eq!(fresh::apply(&other, &root, &other_ch).unwrap(), 1);
            }
        };
        let op = open_fresh_with(&o, 1, false, &mut race)
            .unwrap()
            .expect("index usable");
        assert!(raced);
        let m = read_manifest(&dir).unwrap();
        assert_eq!(
            m.deltas, 2,
            "the skipped delta must be recomputed against the reopened index"
        );
        assert_eq!(op.idx.manifest.deltas, 2);
        let hits = |pat: &str| -> Vec<String> {
            let q = plan::plan(pat, true, false).unwrap();
            let mut v: Vec<String> = op
                .idx
                .candidates(&q)
                .iter()
                .map(|id| op.idx.path(id).unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(hits("omega_one"), ["src/main.rs"]);
        assert_eq!(hits("omega_two"), ["src/lib.rs"]);
        assert!(
            fresh::check(&op.idx, &root, Fresh::Stat, 1)
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// A search after an edit answers from the changed files directly (old
    /// versions leave the candidates, deleted files vanish, new files appear)
    /// and leaves the delta to the detached refresh.
    #[test]
    fn deferred_delta_answers_first() {
        use greeg_index::build::{BuildOpts, build};
        let base = std::env::temp_dir().join(format!("greeg-deferred-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { helper_alpha(); }\n").unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn helper_alpha() {}\npub fn omega_gone() {}\n",
        )
        .unwrap();
        for i in 0..60 {
            fs::write(
                root.join(format!("src/f{i}.rs")),
                format!("pub fn filler_{i}() {{}}\n"),
            )
            .unwrap();
        }
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
        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: Fresh::Stat,
            pattern: "omega".to_string(),
            ..Default::default()
        };
        let rels =
            |r: &ScanResult| -> Vec<String> { r.files.iter().map(|f| f.rel.clone()).collect() };
        let r = crate::scan(&o).unwrap();
        assert_eq!(r.stats.source, "index");
        assert_eq!(rels(&r), ["src/lib.rs"]);
        assert_eq!(
            r.rung,
            Rung::Exact,
            "near-misses of a bare identifier answer on rung 1"
        );
        assert_eq!(r.stats.fresh_deferred, 0);

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("src/main.rs"), "fn main() { omega_one(); }\n").unwrap();
        fs::remove_file(root.join("src/lib.rs")).unwrap();
        fs::write(root.join("src/new.rs"), "fn omega_two() {}\n").unwrap();

        let r = crate::scan(&o).unwrap();
        assert_eq!(r.stats.source, "index");
        assert_eq!(r.stats.fresh_deferred, 3, "modified + added + deleted");
        assert_eq!(rels(&r), ["src/main.rs", "src/new.rs"]);
        assert_eq!(r.files[0].hits[0].line, 1);
        assert!(
            r.files.iter().all(|f| f.file_id.is_none()),
            "read from disk, not the index"
        );
        assert_eq!(
            read_manifest(&dir).unwrap().deltas,
            0,
            "the delta is published after the answer, not before"
        );

        // the detached step
        refresh_now(&root, &dir, 1).unwrap();
        assert_eq!(read_manifest(&dir).unwrap().deltas, 1);
        let r = crate::scan(&o).unwrap();
        assert_eq!(r.stats.fresh_deferred, 0);
        assert_eq!(rels(&r), ["src/main.rs", "src/new.rs"]);
        assert!(r.files.iter().all(|f| f.file_id.is_some()));
        // the queued refresh must not leak into other tests as a real spawn
        let _ = PENDING_BUILD.lock().unwrap().take();
        let _ = fs::remove_dir_all(&base);
    }

    /// Whole-word queries plan from the word postings: only the files holding
    /// the word are opened, the near-misses come from the dictionary, and
    /// parity mode keeps the trigram plan.
    #[test]
    fn word_plan_opens_only_the_files_with_the_word() {
        use greeg_index::build::{BuildOpts, build};
        let base = std::env::temp_dir().join(format!("greeg-words-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn alpha_beta() { alphabet(); }\n").unwrap();
        fs::write(
            root.join("src/c.rs"),
            "fn alphabet() {}\nfn other() { alpha(); }\n",
        )
        .unwrap();
        fs::write(root.join("src/d.rs"), "// nothing here\n").unwrap();
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
        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: Fresh::None,
            pattern: "alpha".to_string(),
            word: true,
            ..Default::default()
        };
        let rels =
            |r: &ScanResult| -> Vec<String> { r.files.iter().map(|f| f.rel.clone()).collect() };
        let r = crate::scan(&o).unwrap();
        assert!(
            r.stats.plan.starts_with("words [\"alpha\"]"),
            "{}",
            r.stats.plan
        );
        assert_eq!(r.stats.candidates, 2, "a.rs and c.rs hold the word");
        assert_eq!(rels(&r), ["src/a.rs", "src/c.rs"]);

        // a bare identifier in a ranked layout: the whole word, plus the
        // dictionary's near-misses with file counts
        let o2 = Options {
            word: false,
            ..o.clone()
        };
        let r = crate::scan(&o2).unwrap();
        assert!(r.stats.plan.starts_with("words"), "{}", r.stats.plan);
        assert_eq!(rels(&r), ["src/a.rs", "src/c.rs"]);
        assert_eq!(
            r.related_index,
            vec![("alphabet".to_string(), 2), ("alpha_beta".to_string(), 1)]
        );

        // parity mode keeps ripgrep's substring semantics through the grams
        let o3 = Options {
            budget: 0,
            ..o2.clone()
        };
        let r = crate::scan(&o3).unwrap();
        assert!(!r.stats.plan.starts_with("words"), "{}", r.stats.plan);
        assert_eq!(rels(&r), ["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert!(r.related_index.is_empty());

        // an unindexed word opens nothing
        let o4 = Options {
            pattern: "gamma".to_string(),
            ..o.clone()
        };
        let r = crate::scan(&o4).unwrap();
        assert_eq!(r.stats.candidates, 0);
        assert!(r.files.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    /// The delta a refresh publishes carries word postings: a whole-word query
    /// answers a word that exists only in the edited files from the delta, the
    /// superseded version's words are gone, and the dictionary's near-misses
    /// include the delta's words.
    #[test]
    fn delta_word_postings_answer_after_refresh() {
        use greeg_index::build::{BuildOpts, build};
        let base = std::env::temp_dir().join(format!("greeg-delta-words-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { helper_alpha(); }\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn helper_alpha() {}\n").unwrap();
        for i in 0..60 {
            fs::write(
                root.join(format!("src/f{i}.rs")),
                format!("pub fn filler_{i}() {{}}\n"),
            )
            .unwrap();
        }
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
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("src/main.rs"), "fn main() { zeta_new(); }\n").unwrap();
        fs::write(root.join("src/new.rs"), "fn zeta_other() {}\n").unwrap();
        refresh_now(&root, &dir, 1).unwrap();
        assert_eq!(read_manifest(&dir).unwrap().deltas, 1);

        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: Fresh::None,
            pattern: "zeta_new".to_string(),
            word: true,
            ..Default::default()
        };
        let rels =
            |r: &ScanResult| -> Vec<String> { r.files.iter().map(|f| f.rel.clone()).collect() };
        let r = crate::scan(&o).unwrap();
        assert!(r.stats.plan.starts_with("words"), "{}", r.stats.plan);
        assert_eq!(
            r.stats.candidates, 1,
            "the delta's postings name main.rs only"
        );
        assert_eq!(rels(&r), ["src/main.rs"]);
        assert!(r.files[0].file_id.is_some(), "answered from the delta");

        // the superseded main.rs held `helper_alpha`; its postings are tombstoned
        let r = crate::scan(&Options {
            pattern: "helper_alpha".to_string(),
            ..o.clone()
        })
        .unwrap();
        assert_eq!(r.stats.candidates, 1);
        assert_eq!(rels(&r), ["src/lib.rs"]);

        // a bare identifier no file holds as a whole word: the trigram plan
        // opens the near-misses on this rung (base and delta), no ladder climb
        let r = crate::scan(&Options {
            pattern: "zeta".to_string(),
            word: false,
            ..o.clone()
        })
        .unwrap();
        assert!(!r.stats.plan.starts_with("words"), "{}", r.stats.plan);
        assert_eq!(r.rung, Rung::Exact);
        assert_eq!(rels(&r), ["src/main.rs", "src/new.rs"]);
        assert!(r.related_index.is_empty());
        // Only discovery may retry a whole-word miss as a substring.
        let r = crate::scan(&Options {
            pattern: "zeta".to_string(),
            ..o.clone()
        })
        .unwrap();
        assert_eq!(r.rung, Rung::Exact);
        assert!(r.files.is_empty());
        let r = crate::scan(&Options {
            pattern: "zeta".to_string(),
            matching: crate::MatchingPolicy::Discover,
            ..o.clone()
        })
        .unwrap();
        assert_eq!(r.rung, Rung::NoWord);
        assert_eq!(rels(&r), ["src/main.rs", "src/new.rs"]);
        let _ = fs::remove_dir_all(&base);
    }

    /// One refresher per index: a live `REFRESHING` marker makes `refresh_now`
    /// a no-op, a stale one (its refresher died) is taken over and removed.
    #[test]
    fn refresh_marker_serializes_refreshers() {
        use greeg_index::build::{BuildOpts, build};
        let base =
            std::env::temp_dir().join(format!("greeg-refresh-marker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        for i in 0..60 {
            fs::write(
                root.join(format!("src/f{i}.rs")),
                format!("pub fn filler_{i}() {{}}\n"),
            )
            .unwrap();
        }
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
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("src/f0.rs"), "pub fn filler_changed() {}\n").unwrap();
        let marker = dir.join("REFRESHING");
        fs::write(&marker, "1\n").unwrap();
        refresh_now(&root, &dir, 1).unwrap();
        assert_eq!(
            read_manifest(&dir).unwrap().deltas,
            0,
            "another refresher owns it"
        );
        assert!(marker.exists());
        // the owner died: a marker older than thirty seconds is taken over
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        fs::OpenOptions::new()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        refresh_now(&root, &dir, 1).unwrap();
        assert_eq!(read_manifest(&dir).unwrap().deltas, 1);
        assert!(!marker.exists(), "released after the publish");
        // nothing pending: a second refresh publishes nothing and releases again
        refresh_now(&root, &dir, 1).unwrap();
        assert_eq!(read_manifest(&dir).unwrap().deltas, 1);
        assert!(!marker.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn path_allowed_prefixes() {
        let p = vec!["tokio/src".to_string()];
        assert!(path_allowed("tokio/src/lib.rs", &p));
        assert!(path_allowed("tokio/src", &p));
        assert!(!path_allowed("tokio/src-old/lib.rs", &p));
        assert!(!path_allowed("tokio-util/src/lib.rs", &p));
        assert!(path_allowed("anything", &[]));
        assert!(path_allowed("anything", &[".".to_string()]));
        assert!(path_allowed("anything", &[String::new()]));
    }

    #[test]
    fn a_file_named_on_the_command_line_is_searched_even_when_ignored() {
        use crate::{Options, Rung};
        use greeg_index::build::{BuildOpts, build};
        let base =
            std::env::temp_dir().join(format!("greeg-explicit-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
        std::fs::write(root.join("vendor/dep.rs"), "pub fn needle_xyz() {}\n").unwrap();
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
        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: greeg_index::fresh::Mode::None,
            pattern: "needle_xyz".into(),
            ..Default::default()
        };
        // Discovery counts ignored matches without returning them as hits.
        let r = crate::scan(&o).unwrap();
        assert_eq!(r.stats.total_hits, 0);
        assert!(r.ignored_only.is_none());
        let r = crate::scan(&Options {
            matching: crate::MatchingPolicy::Discover,
            ..o.clone()
        })
        .unwrap();
        assert_eq!(r.stats.total_hits, 0);
        assert!(r.ignored_only.is_some());
        // named on the command line, the file is searched (as ripgrep does), by scan mode
        let r = crate::scan(&Options {
            paths: vec![root.join("vendor/dep.rs")],
            ..o.clone()
        })
        .unwrap();
        assert_eq!(r.stats.total_hits, 1);
        assert_eq!(r.rung, Rung::Exact);
        assert!(
            r.files[0].rel.ends_with("vendor/dep.rs"),
            "{}",
            r.files[0].rel
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rel_of_handles_absolute_and_dotted_paths() {
        let dir = std::env::temp_dir().join(format!("greeg-rel-{}", std::process::id()));
        fs::create_dir_all(dir.join("src/sync")).unwrap();
        fs::write(dir.join("src/sync/a.rs"), "x").unwrap();
        assert_eq!(
            rel_of(&dir, &dir.join("src/sync")).as_deref(),
            Some("src/sync")
        );
        assert_eq!(
            rel_of(&dir, &dir.join("src/sync/a.rs")).as_deref(),
            Some("src/sync/a.rs")
        );
        assert_eq!(rel_of(&dir, &dir).as_deref(), Some(""));
        assert_eq!(
            rel_of(&dir, Path::new("./src/sync/")).as_deref(),
            Some("src/sync")
        );
        assert_eq!(
            rel_of(&dir, Path::new("src/sync")).as_deref(),
            Some("src/sync")
        );
        assert_eq!(rel_of(&dir, Path::new(".")).as_deref(), Some(""));
        // outside the root (or nonexistent): scan mode
        assert_eq!(rel_of(&dir, &std::env::temp_dir()), None);
        assert_eq!(rel_of(&dir, &dir.join("missing/../nowhere")), None);
        let abs = rel_of(&dir, &dir.join("src/sync")).unwrap();
        assert!(path_allowed("src/sync/a.rs", &[abs]));
    }
}
