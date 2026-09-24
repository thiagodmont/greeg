//! Symbol verbs: `def`, `refs`, `callers`, `impls`,
//! `outline`, `map`, `impact`. Each verb returns plain data; rendering lives
//! in the CLI. All verbs work without an index (slower, via scan mode) except
//! `map`, which needs the symbol table.

use crate::indexed::{self, defs_of};
use crate::outcome::Outcome;
use crate::select::Selection;
use crate::{DefSummary, HitKind, Mode, Options, Rung, ScanResult, scan};
use anyhow::{Context, Result, bail};
use greeg_index::Index;
use greeg_index::index::SymId;
use greeg_index::symtab::{kind_from_code, kind_weight};
use greeg_lang::sym::{SYM_EXPORTED, SYM_HAS_DOC, SYM_OBJ_MEMBER, SYM_TEST};
use greeg_lang::{DefKind, FileFlags, Lang};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

/// One definition, fully described for output.
#[derive(Clone, Debug)]
pub struct DefEntry {
    pub rel: String,
    pub line: u32,
    pub kind: DefKind,
    pub name: String,
    /// Enclosing chain, outermost first, excluding the symbol itself.
    pub chain: Vec<(DefKind, String)>,
    pub signature: String,
    pub doc: Option<String>,
    pub flags: u8,
    pub file_flags: FileFlags,
    pub supers: Vec<String>,
    pub score: f32,
    /// Reachability from the origin (1.0 direct import … 0.4 unrelated).
    pub reach: f32,
    pub start: u32,
    pub end: u32,
    pub file_id: Option<u32>,
    /// The file itself is the module (`sleep.rs`, `pkg/__init__.py`), line 1.
    pub file_module: bool,
}

#[derive(Debug, Default)]
pub struct DefResult {
    pub name: String,
    pub entries: Vec<DefEntry>,
    pub total: usize,
    pub source: &'static str,
    pub rung: Rung,
    pub elapsed_ms: f64,
    pub fresh: &'static str,
    /// Names suggested by the ladder when nothing matched exactly.
    pub suggestions: Vec<String>,
}

fn loc_w(flags: FileFlags, rel: &str, all: bool) -> f32 {
    crate::loc_weight(flags, rel, all)
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Import reachability: 1.0 direct, 0.8 within two hops,
/// 0.6 same directory, 0.4 otherwise. `origins` are file ids.
pub fn reach(idx: &Index, origins: &[u32], target: u32) -> f32 {
    if origins.is_empty() {
        return 0.6;
    }
    let trel = idx.path(target).unwrap_or("");
    let target = idx.latest(target);
    let mut best = 0.4f32;
    for &o in origins {
        if o == target {
            return 1.0;
        }
        // edges follow edits: an edited origin or target keeps its imports
        let out = idx.out_edges(o);
        if out.contains(&target) {
            return 1.0;
        }
        for &mid in out.iter().take(64) {
            if idx.out_edges(mid).contains(&target) {
                best = best.max(0.8);
                break;
            }
        }
        // reverse direction: the target imports the origin (siblings that share code)
        if best < 0.8 && idx.in_edges(o).contains(&target) {
            best = best.max(0.8);
        }
        if dir_of(idx.path(o).unwrap_or("")) == dir_of(trel) {
            best = best.max(0.6);
        }
    }
    best
}

/// Origin file ids for `--from` or the session focus.
pub fn origin_ids(idx: &Index, from: &[String]) -> Vec<u32> {
    let mut ids = Vec::new();
    if from.is_empty() {
        return ids;
    }
    for (id, rel, _) in idx.live_files() {
        if from.iter().any(|f| f.trim_start_matches("./") == rel) {
            ids.push(id);
        }
    }
    ids
}

/// Kind weight of a file module in `def` (`symtab::kind_weight` covers
/// symbols): below every symbol kind, so the file is the answer only when
/// nothing declares the name (scip-python and scip-typescript do not count a
/// module file as a definition; rust-analyzer does).
const FILE_MODULE_W: f32 = 0.3;

/// Signature and doc of a file module: the language's module keyword plus the
/// name, and the first line of a leading `//!` / `/*!` / `/**` comment or
/// module docstring.
fn module_signature_and_doc(src: &[u8], lang: Lang, name: &str) -> (String, Option<String>) {
    let kw = match lang {
        Lang::Rust => "mod",
        Lang::Kotlin => "file",
        _ => "module",
    };
    let mut doc = None;
    for line in src.split(|&b| b == b'\n').take(3) {
        let t = line.trim_ascii();
        if t.is_empty() || t.starts_with(b"#!") || t.starts_with(b"#![") || t.starts_with(b"# -*-")
        {
            continue;
        }
        let body = if let Some(r) = t.strip_prefix(b"//!") {
            r
        } else if let Some(r) = t.strip_prefix(b"/*!").or_else(|| t.strip_prefix(b"/**")) {
            r.split(|&b| b == b'*').next().unwrap_or(b"")
        } else if let Some(r) = t.strip_prefix(b"\"\"\"").or_else(|| t.strip_prefix(b"'''")) {
            r.split(|&b| b == b'"' || b == b'\'').next().unwrap_or(b"")
        } else {
            break;
        };
        let s = String::from_utf8_lossy(body.trim_ascii()).to_string();
        if !s.is_empty() {
            let cut: String = s.chars().take(120).collect();
            doc = Some(cut);
        }
        break;
    }
    (format!("{kw} {name}"), doc)
}

/// Signature line and doc first line for a definition at `start` in `src`.
fn signature_and_doc(
    idx: Option<(&Index, u32)>,
    src: &[u8],
    start: u32,
    flags: u8,
    lang: Lang,
) -> (String, Option<String>) {
    let s = start as usize;
    let le = memchr::memchr(b'\n', &src[s.min(src.len())..])
        .map(|k| s + k)
        .unwrap_or(src.len());
    let sig = String::from_utf8_lossy(src[s..le].trim_ascii()).to_string();
    let sig = if sig.len() > 200 {
        format!(
            "{}…",
            &sig[..sig.char_indices().nth(197).map(|(i, _)| i).unwrap_or(197)]
        )
    } else {
        sig
    };
    if flags & SYM_HAS_DOC == 0 {
        return (sig, None);
    }
    let doc_span = idx.and_then(|(idx, id)| {
        // comment right before the definition, or a Python docstring right after
        let mut probe = start.saturating_sub(1);
        let mut n = 0;
        while probe > 0 && n < 200 {
            if let Some((k, a, b)) = idx.noncode_at(id, probe) {
                if k == 0 || k == 2 {
                    // walk back over a block of consecutive line comments (`///` per line)
                    let (mut fa, fb) = (a, b);
                    let mut p = a;
                    while p > 0 {
                        let q = p - 1;
                        let c = src[q as usize];
                        if c == b'\n' || c == b' ' || c == b'\t' || c == b'\r' {
                            p = q;
                            continue;
                        }
                        match idx.noncode_at(id, q) {
                            Some((k2, a2, _))
                                if (k2 == 0 || k2 == 2)
                                    && src[q as usize..p as usize]
                                        .iter()
                                        .filter(|&&x| x == b'\n')
                                        .count()
                                        <= 1 =>
                            {
                                fa = a2;
                                p = a2;
                            }
                            _ => break,
                        }
                    }
                    return Some((fa, fb));
                }
                break;
            }
            let c = src[probe as usize];
            if !(c.is_ascii_whitespace()
                || c == b'#'
                || c == b'@'
                || c == b'['
                || c == b']'
                || c.is_ascii_alphanumeric()
                || c == b'_'
                || c == b'('
                || c == b')'
                || c == b'.'
                || c == b'='
                || c == b'"'
                || c == b',')
            {
                break;
            }
            probe -= 1;
            n += 1;
        }
        if lang == Lang::Python {
            let body = &src[s..src.len().min(s + 4096)];
            if let Some(q) =
                memchr::memmem::find(body, b"\"\"\"").or_else(|| memchr::memmem::find(body, b"'''"))
            {
                let a = s + q;
                if let Some((2, x, y)) = idx.noncode_at(id, a as u32) {
                    return Some((x, y));
                }
            }
        }
        None
    });
    let doc = doc_span.map(|(a, b)| doc_first_line(&src[a as usize..b as usize]));
    (sig, doc.filter(|d| !d.is_empty()))
}

fn doc_first_line(t: &[u8]) -> String {
    for line in t.split(|&b| b == b'\n') {
        let l = String::from_utf8_lossy(line);
        let l = l
            .trim()
            .trim_start_matches("/**")
            .trim_start_matches("/*!")
            .trim_start_matches("///")
            .trim_start_matches("//!")
            .trim_start_matches("/*")
            .trim_start_matches('*')
            .trim_start_matches('#')
            .trim_start_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_start_matches("r\"\"\"")
            .trim()
            .trim_end_matches("*/")
            .trim_end_matches("\"\"\"")
            .trim_end_matches("'''")
            .trim();
        if !l.is_empty() {
            return l.chars().take(120).collect();
        }
    }
    String::new()
}

/// Implementations kept (and extras, a fifth of it) under a budget.
const DESCRIBED_IMPLS: usize = 200;

/// Definitions ranked on metadata before any is described (read for its
/// signature); `--budget 0` describes them all.
const DESCRIBED_DEFS: usize = 256;

/// A definition's ranking score, from the index alone.
fn def_score(idx: &Index, s: SymId, fid: u32, rel: &str, all: bool, reach: f32) -> f32 {
    let Some(r) = idx.sym(s) else { return 0.0 };
    let fflags = FileFlags(idx.rec(fid).map(|r| r.flags).unwrap_or(0));
    let exported = if r.flags & SYM_EXPORTED != 0 {
        1.0
    } else {
        0.85
    };
    // a declaration inside a function body is a closure, not the
    // definition an agent asks for when a top-level one exists
    let nested = if r.flags & SYM_OBJ_MEMBER != 0 {
        // an object-literal member: usually an implementation of a typed member
        0.6
    } else if idx
        .sym_parent(s)
        .and_then(|p| idx.sym(p))
        .is_some_and(|p| matches!(kind_from_code(p.kind), DefKind::Function | DefKind::Method))
    {
        0.7
    } else {
        1.0
    };
    kind_weight(r.kind)
        * exported
        * nested
        * loc_w(fflags, rel, all)
        * (0.6 + 0.4 * idx.rank(fid))
        * reach
}

/// Does the request select the indexed file `fid`?
fn selected(idx: &Index, sel: &Selection, fid: u32) -> bool {
    idx.rec(fid)
        .is_some_and(|r| sel.selects(idx.path(fid).unwrap_or(""), FileFlags(r.flags)))
}

fn kind_filter_ok(kinds: &[HitKind], _k: DefKind) -> bool {
    kinds.is_empty() || kinds.contains(&HitKind::Def)
}

fn case_variant_names(idx: &Index, name: &str) -> Vec<String> {
    let Some(first) = name.chars().next() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for (_, seg) in idx.segments() {
        if let Some(sv) = seg.symbols() {
            for prefix in [first.to_ascii_lowercase(), first.to_ascii_uppercase()] {
                for nid in sv.prefix_range(&prefix.to_string()) {
                    let candidate = sv.name(nid);
                    if candidate.eq_ignore_ascii_case(name) && !names.iter().any(|n| n == candidate)
                    {
                        names.push(candidate.to_string());
                    }
                }
            }
        }
    }
    names
}

/// `greeg def NAME`.
pub fn def(
    o: &Options,
    name: &str,
    from: &[String],
    want_kind: Option<DefKind>,
) -> Result<DefResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    let mut res = DefResult {
        name: name.to_string(),
        ..Default::default()
    };
    let sel = Selection::new(o, Vec::new())?;
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
        // Fall back for truncated Unicode names; retain existing symbol/module results.
        && (name.is_ascii()
            || !op.idx.lookup(name).is_empty()
            || !op.idx.module_files(name, 1).is_empty())
        && let Some(also) = sel.coverage(&op.idx, op.pending.as_ref())
    {
        let idx = &op.idx;
        res.source = "index";
        res.fresh = op.fresh_method;
        let selected = |fid: u32| selected(idx, &sel, fid);
        let mut syms = idx.lookup(name);
        let fold_case =
            o.case_insensitive || (o.smart_case && !name.chars().any(|c| c.is_uppercase()));
        if fold_case {
            for candidate in case_variant_names(idx, name) {
                if candidate != name {
                    syms.extend(idx.lookup(&candidate));
                }
            }
        }
        syms.retain(|s| selected(idx.sym_file(*s)));
        // the file that is the module: listed after every symbol; an agent
        // wants to open it when nothing else declares the name
        let mut file_mods = idx.module_files(name, usize::MAX);
        file_mods.retain(|&f| selected(f));
        let mut rung = Rung::Exact;
        if syms.is_empty() && file_mods.is_empty() && o.matching == crate::MatchingPolicy::Discover
        {
            // ladder over names: case-insensitive, split tokens, fuzzy
            let mut alts = case_variant_names(idx, name);
            if !alts.is_empty() {
                rung = Rung::CaseInsensitive;
            } else {
                let toks = greeg_index::symtab::split_tokens(name);
                if toks.len() >= 2 {
                    alts = idx.names_with_tokens(&toks, 8);
                    if !alts.is_empty() {
                        rung = Rung::SplitTokens(alts.clone());
                    }
                }
                if alts.is_empty() {
                    let d = if name.len() < 8 { 1 } else { 2 };
                    alts = idx
                        .fuzzy_names(name, d, 8)
                        .into_iter()
                        .map(|(n, _)| n)
                        .collect();
                    if !alts.is_empty() {
                        rung = Rung::Fuzzy(alts.clone());
                    }
                }
            }
            for a in &alts {
                syms.extend(
                    idx.lookup(a)
                        .into_iter()
                        .filter(|s| selected(idx.sym_file(*s))),
                );
            }
            res.suggestions = alts;
        }
        res.rung = rung;
        if let Some(wk) = want_kind {
            syms.retain(|s| idx.sym(*s).is_some_and(|r| kind_from_code(r.kind) == wk));
            if wk != DefKind::Module {
                file_mods.clear();
            }
        }
        res.total = syms.len() + file_mods.len();
        let origins = origin_ids(idx, from);
        // rank every eligible definition on metadata, then describe the best
        // ones: all of them without a budget
        let mut ranked: Vec<(f32, SymId, &str, u32, f32)> = syms
            .iter()
            .filter_map(|s| {
                let r = idx.sym(*s)?;
                let fid = idx.sym_file(*s);
                let rel = idx.path(fid).unwrap_or("");
                let rch = reach(idx, &origins, fid);
                let score = def_score(idx, *s, fid, rel, o.all, rch);
                Some((score, *s, rel, r.line, rch))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(b.2))
                .then(a.3.cmp(&b.3))
        });
        if o.budget != 0 {
            ranked.truncate(DESCRIBED_DEFS);
        }
        let mut entries: Vec<DefEntry> = Vec::with_capacity(ranked.len() + file_mods.len());
        for &(score, s, rel, _, rch) in &ranked {
            let Some(r) = idx.sym(s) else { continue };
            let fid = idx.sym_file(s);
            let rec = idx.rec(fid).context("file record")?;
            let chain: Vec<(DefKind, String)> = idx
                .sym_chain(s)
                .into_iter()
                .map(|(k, n)| (kind_from_code(k), n.to_string()))
                .collect();
            let chain = chain[..chain.len().saturating_sub(1)].to_vec();
            entries.push(DefEntry {
                rel: rel.to_string(),
                line: r.line,
                kind: kind_from_code(r.kind),
                name: idx.sym_name(s).to_string(),
                chain,
                signature: String::new(),
                doc: None,
                flags: r.flags,
                file_flags: FileFlags(rec.flags),
                supers: idx.sym_supers(s).iter().map(|s| s.to_string()).collect(),
                score,
                reach: rch,
                start: r.start,
                end: r.end,
                file_id: Some(fid),
                file_module: false,
            });
        }
        for &fid in &file_mods {
            let rec = idx.rec(fid).context("file record")?;
            let fflags = FileFlags(rec.flags);
            let rel = idx.path(fid).unwrap_or("").to_string();
            let rch = reach(idx, &origins, fid);
            let score =
                FILE_MODULE_W * loc_w(fflags, &rel, o.all) * (0.6 + 0.4 * idx.rank(fid)) * rch;
            entries.push(DefEntry {
                rel,
                line: 1,
                kind: DefKind::Module,
                name: name.to_string(),
                chain: Vec::new(),
                signature: String::new(),
                doc: None,
                flags: SYM_EXPORTED,
                file_flags: fflags,
                supers: Vec::new(),
                score,
                reach: rch,
                start: 0,
                end: rec.size.min(u32::MAX as u64) as u32,
                file_id: Some(fid),
                file_module: true,
            });
        }
        // selected files the index skipped: found by the scan's definition rules
        if !also.is_empty() {
            let mut so = o.clone();
            so.paths = also
                .iter()
                .map(|r| o.root.join(r))
                .filter(|p| p.is_file())
                .collect();
            so.use_index = false;
            so.matching = crate::MatchingPolicy::Exact;
            if !so.paths.is_empty() {
                let (found, _, _) = scan_defs(&so, name, want_kind)?;
                res.total += found.len();
                entries.extend(found);
            }
        }
        entries.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.rel.cmp(&b.rel))
                .then(a.line.cmp(&b.line))
        });
        // impl blocks are secondary: keep at most three unless they are all there is
        if entries.iter().any(|e| e.kind != DefKind::Impl) {
            let mut n_impl = 0;
            entries.retain(|e| {
                if e.kind == DefKind::Impl {
                    n_impl += 1;
                    n_impl <= 3
                } else {
                    true
                }
            });
        }
        // signatures and docs for the entries that will be shown (bounded)
        let show = if o.budget == 0 {
            entries.len()
        } else {
            (o.budget / 40).clamp(3, 40)
        };
        let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for e in entries.iter_mut().take(show) {
            let src = cache
                .entry(e.rel.clone())
                .or_insert_with(|| greeg_lang::read_text(o.root.join(&e.rel)).unwrap_or_default());
            if src.is_empty() || e.start as usize >= src.len() {
                continue;
            }
            if e.file_module {
                let (sig, doc) =
                    module_signature_and_doc(src, Lang::from_path(Path::new(&e.rel)), &e.name);
                e.signature = sig;
                e.doc = doc;
                continue;
            }
            let (sig, doc) = signature_and_doc(
                e.file_id.map(|id| (idx, id)),
                src,
                e.start,
                e.flags,
                Lang::from_path(Path::new(&e.rel)),
            );
            e.signature = sig;
            e.doc = doc;
        }
        entries.truncate(show.max(1));
        res.entries = entries;
        res.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
        return Ok(res);
    }
    // scan fallback: definition hits by regex
    let (mut entries, rung, source) = scan_defs(o, name, want_kind)?;
    res.source = source;
    res.rung = rung;
    res.total = entries.len();
    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rel.cmp(&b.rel))
    });
    let show = if o.budget == 0 {
        entries.len()
    } else {
        (o.budget / 40).clamp(3, 40)
    };
    entries.truncate(show.max(1));
    res.entries = entries;
    res.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(res)
}

/// Definitions of `name` found by a scan's definition rules, unsorted, with
/// the rung and source that answered.
fn scan_defs(
    o: &Options,
    name: &str,
    want_kind: Option<DefKind>,
) -> Result<(Vec<DefEntry>, Rung, &'static str)> {
    let mut so = o.clone();
    so.use_index &= name.is_ascii();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = vec![HitKind::Def];
    so.mode = Mode::Content;
    so.budget = 0;
    let mut r = scan(&so)?;
    let idxs: Vec<usize> = (0..r.files.len().min(200)).collect();
    crate::refine(&mut r, &idxs);
    let source = if r.stats.source == "index" {
        "index (phase 1)"
    } else {
        "scan"
    };
    let mut entries = Vec::new();
    for f in &r.files {
        for h in &f.hits {
            if h.kind != HitKind::Def {
                continue;
            }
            let (kind, chain, dstart, dend) = match h.def_idx.map(|d| &f.defs[d as usize]) {
                Some(d) => (
                    d.kind,
                    h.chain[..h.chain.len().saturating_sub(1)].to_vec(),
                    d.start,
                    d.end,
                ),
                None => (
                    DefKind::Function,
                    h.chain.clone(),
                    h.line_start,
                    h.match_end,
                ),
            };
            if let Some(wk) = want_kind
                && wk != kind
            {
                continue;
            }
            let _ = kind_filter_ok(&so.kinds, kind);
            entries.push(DefEntry {
                file_module: false,
                rel: f.rel.clone(),
                line: h.line,
                kind,
                name: name.to_string(),
                chain,
                signature: String::from_utf8_lossy(&h.text).to_string(),
                doc: None,
                flags: 0,
                file_flags: f.flags,
                supers: vec![],
                score: h.score,
                reach: 0.6,
                start: dstart,
                end: dend,
                file_id: None,
            });
        }
    }
    Ok((entries, r.rung.clone(), source))
}

/// `greeg refs NAME`: a word-bounded search, classified, plus the fraction of
/// hits reachable from a definition with confidence ≥ 0.8.
pub struct RefsResult {
    pub scan: ScanResult,
    pub defs: Vec<DefEntry>,
    /// Hits (file idx, hit idx) whose file reaches a definition file with ≥ 0.8.
    pub resolved: usize,
    pub classified: usize,
}

pub fn refs(o: &Options, name: &str, kinds: &[HitKind]) -> Result<RefsResult> {
    let mut so = o.clone();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = kinds.to_vec();
    so.mode = Mode::Content;
    let scan_r = scan(&so)?;
    // the scan above ran the freshness check; the lookups below reuse its result
    let mut d = o.clone();
    d.matching = crate::MatchingPolicy::Exact;
    d.budget = 400;
    d.fresh = greeg_index::fresh::Mode::None;
    let defs = def(&d, name, &[], None)
        .map(|r| r.entries)
        .unwrap_or_default();
    let (mut resolved, mut classified) = (0usize, 0usize);
    if let Some(op) = if o.use_index {
        indexed::open_fresh(&d, 1).ok().flatten()
    } else {
        None
    } && op.idx.has_symbols()
    {
        let def_ids: Vec<u32> = defs.iter().filter_map(|e| e.file_id).collect();
        for f in &scan_r.files {
            let Some(fid) = f.file_id else { continue };
            classified += f.hits.len();
            if reach(&op.idx, &def_ids, fid) >= 0.8 {
                resolved += f.hits.len();
            }
        }
    }
    Ok(RefsResult {
        scan: scan_r,
        defs,
        resolved,
        classified,
    })
}

/// One calling function (`callers`).
#[derive(Clone, Debug)]
pub struct Caller {
    pub rel: String,
    pub chain: Vec<(DefKind, String)>,
    pub def_line: u32,
    pub count: usize,
    pub lines: Vec<u32>,
    pub file_flags: FileFlags,
    pub score: f32,
    /// Depth-2: functions that call this caller (names only).
    pub called_by: Vec<String>,
}

pub struct CallersResult {
    /// Of the call-site search; the caller renders it per function.
    pub outcome: Outcome,
    pub name: String,
    pub callers: Vec<Caller>,
    pub total_hits: usize,
    pub files: usize,
    pub source: &'static str,
    pub elapsed_ms: f64,
    pub rung: Rung,
}

pub fn callers(o: &Options, name: &str, depth: usize) -> Result<CallersResult> {
    let t0 = Instant::now();
    let mut so = o.clone();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = vec![HitKind::Call, HitKind::Member];
    so.mode = Mode::Content;
    so.budget = 0;
    let mut r = scan(&so)?;
    let idxs: Vec<usize> = (0..r.files.len().min(400)).collect();
    crate::refine(&mut r, &idxs);
    let mut map: BTreeMap<(String, Option<u32>), Caller> = BTreeMap::new();
    let mut total = 0usize;
    for f in &r.files {
        if !f.lang.has_grammar() {
            continue;
        }
        for h in &f.hits {
            if h.kind != HitKind::Call && h.kind != HitKind::Member {
                continue;
            }
            total += 1;
            // key by the enclosing *function-like* symbol (walk out of nested blocks is implicit: chains end at the innermost def)
            let (chain, def_line) = match h.def_idx.map(|d| &f.defs[d as usize]) {
                Some(d) => (
                    if d.chain.is_empty() {
                        h.chain.clone()
                    } else {
                        d.chain.clone()
                    },
                    d.line,
                ),
                None => (Vec::new(), h.line),
            };
            let e = map
                .entry((f.rel.clone(), h.def_idx))
                .or_insert_with(|| Caller {
                    rel: f.rel.clone(),
                    chain,
                    def_line,
                    count: 0,
                    lines: Vec::new(),
                    file_flags: f.flags,
                    score: f.prior,
                    called_by: Vec::new(),
                });
            e.count += 1;
            if e.lines.len() < 8 {
                e.lines.push(h.line);
            }
        }
    }
    let mut callers: Vec<Caller> = map.into_values().collect();
    callers.sort_by(|a, b| {
        (b.score * (1.0 + (b.count as f32).ln()))
            .partial_cmp(&(a.score * (1.0 + (a.count as f32).ln())))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rel.cmp(&b.rel))
    });
    if depth >= 2 {
        let mut seen: Vec<String> = Vec::new();
        for c in callers.iter_mut().take(12) {
            let Some((_, cname)) = c.chain.last() else {
                continue;
            };
            if cname == name || seen.contains(cname) {
                continue;
            }
            seen.push(cname.clone());
            let mut so2 = so.clone();
            so2.pattern = cname.clone();
            so2.matching = crate::MatchingPolicy::Exact;
            if let Ok(mut r2) = scan(&so2) {
                let idxs: Vec<usize> = (0..r2.files.len().min(60)).collect();
                crate::refine(&mut r2, &idxs);
                let mut names: Vec<String> = Vec::new();
                for f in &r2.files {
                    for h in &f.hits {
                        if (h.kind == HitKind::Call || h.kind == HitKind::Member)
                            && let Some((_, n)) = h.chain.last()
                            && n != cname
                            && !names.contains(n)
                        {
                            names.push(n.clone());
                        }
                    }
                }
                names.truncate(12);
                c.called_by = names;
            }
        }
    }
    Ok(CallersResult {
        outcome: Outcome::of_search(&r, 0),
        name: name.to_string(),
        files: r.files.len(),
        callers,
        total_hits: total,
        source: r.stats.source,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
        rung: r.rung.clone(),
    })
}

pub struct ImplsResult {
    pub name: String,
    /// From the symbol table's supertype lists, best first.
    pub direct: Vec<DefEntry>,
    /// Every eligible direct implementation (`direct` keeps the best
    /// `DESCRIBED_IMPLS` unless the budget is unlimited).
    pub direct_total: usize,
    /// Type-position hits on definition lines that the resolver did not tie to a supertype list.
    pub extras: Vec<DefEntry>,
    pub extras_total: usize,
    pub source: &'static str,
    pub fresh: &'static str,
    pub elapsed_ms: f64,
}

pub fn impls(o: &Options, name: &str) -> Result<ImplsResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    let mut direct = Vec::new();
    let mut direct_total = 0;
    let mut source = "scan";
    let mut fresh = "";
    let mut have: Vec<(String, u32)> = Vec::new();
    let sel = Selection::new(o, Vec::new())?;
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
        // skipped files hold no symbols; the type-position scan below reads them
        && sel.coverage(&op.idx, op.pending.as_ref()).is_some()
    {
        let idx = &op.idx;
        source = "index";
        fresh = op.fresh_method;
        let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        // rank every eligible implementation before reading any source
        let mut found: Vec<(f32, SymId)> = idx
            .implementors(name)
            .into_iter()
            .filter(|s| selected(idx, &sel, idx.sym_file(*s)))
            .filter_map(|s| {
                let r = idx.sym(s)?;
                let fid = idx.sym_file(s);
                let fflags = FileFlags(idx.rec(fid).map(|r| r.flags).unwrap_or(0));
                let rel = idx.path(fid).unwrap_or("");
                Some((
                    kind_weight(r.kind) * loc_w(fflags, rel, o.all) * (0.6 + 0.4 * idx.rank(fid)),
                    s,
                ))
            })
            .collect();
        direct_total = found.len();
        found.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    idx.path(idx.sym_file(a.1))
                        .cmp(&idx.path(idx.sym_file(b.1))),
                )
        });
        if o.budget != 0 {
            found.truncate(DESCRIBED_IMPLS);
        }
        for (_, s) in found {
            let Some(r) = idx.sym(s) else { continue };
            let fid = idx.sym_file(s);
            let rel = idx.path(fid).unwrap_or("").to_string();
            let fflags = FileFlags(idx.rec(fid).map(|r| r.flags).unwrap_or(0));
            let chain: Vec<(DefKind, String)> = idx
                .sym_chain(s)
                .into_iter()
                .map(|(k, n)| (kind_from_code(k), n.to_string()))
                .collect();
            let chain = chain[..chain.len().saturating_sub(1)].to_vec();
            let src = cache
                .entry(rel.clone())
                .or_insert_with(|| greeg_lang::read_text(o.root.join(&rel)).unwrap_or_default());
            let (sig, doc) = if (r.start as usize) < src.len() {
                signature_and_doc(
                    Some((idx, fid)),
                    src,
                    r.start,
                    r.flags,
                    Lang::from_path(Path::new(&rel)),
                )
            } else {
                (String::new(), None)
            };
            let score =
                kind_weight(r.kind) * loc_w(fflags, &rel, o.all) * (0.6 + 0.4 * idx.rank(fid));
            have.push((rel.clone(), r.line));
            direct.push(DefEntry {
                file_module: false,
                rel,
                line: r.line,
                kind: kind_from_code(r.kind),
                name: idx.sym_name(s).to_string(),
                chain,
                signature: sig,
                doc,
                flags: r.flags,
                file_flags: fflags,
                supers: idx.sym_supers(s).iter().map(|x| x.to_string()).collect(),
                score,
                reach: 0.6,
                start: r.start,
                end: r.end,
                file_id: Some(fid),
            });
        }
        direct.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.rel.cmp(&b.rel))
        });
    }
    // extras: type-position hits on definition lines
    let mut so = o.clone();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = vec![HitKind::Type];
    so.mode = Mode::Content;
    so.budget = 0;
    so.matching = crate::MatchingPolicy::Exact;
    let mut extras = Vec::new();
    if let Ok(mut r) = scan(&so) {
        let idxs: Vec<usize> = (0..r.files.len().min(200)).collect();
        crate::refine(&mut r, &idxs);
        for f in &r.files {
            for h in &f.hits {
                if h.kind != HitKind::Type {
                    continue;
                }
                let lang = f.lang;
                let line = &h.text;
                let Some((ns, ne)) = greeg_lang::defs::def_name_on_line(lang, line) else {
                    continue;
                };
                let dname = String::from_utf8_lossy(&line[ns..ne]).to_string();
                if dname == name || have.contains(&(f.rel.clone(), h.line)) {
                    continue;
                }
                let kind = h
                    .def_idx
                    .map(|d| f.defs[d as usize].kind)
                    .unwrap_or(DefKind::Class);
                if !kind.is_container() && kind != DefKind::TypeAlias {
                    continue;
                }
                extras.push(DefEntry {
                    file_module: false,
                    rel: f.rel.clone(),
                    line: h.line,
                    kind,
                    name: dname,
                    chain: h.chain[..h.chain.len().saturating_sub(1)].to_vec(),
                    signature: String::from_utf8_lossy(line).to_string(),
                    doc: None,
                    flags: 0,
                    file_flags: f.flags,
                    supers: vec![name.to_string()],
                    score: h.score,
                    reach: 0.6,
                    start: h.line_start,
                    end: h.match_end,
                    file_id: f.file_id,
                });
            }
        }
        extras.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.rel.cmp(&b.rel))
        });
    }
    let extras_total = extras.len();
    if o.budget != 0 {
        extras.truncate(DESCRIBED_IMPLS / 5);
    }
    Ok(ImplsResult {
        name: name.to_string(),
        direct,
        direct_total,
        extras,
        extras_total,
        source,
        fresh,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
    })
}

pub struct OutlineResult {
    pub rel: String,
    pub defs: Vec<DefSummary>,
    pub imports: Vec<String>,
    pub source: &'static str,
    pub lang: Lang,
    pub elapsed_ms: f64,
    pub parse_errors: bool,
}

/// `greeg outline FILE`: the definitions of one file as a tree.
pub fn outline(o: &Options, file: &str) -> Result<OutlineResult> {
    let t0 = Instant::now();
    let rel = file.trim_start_matches("./").to_string();
    let path = o.root.join(&rel);
    let lang = Lang::from_path(&path);
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
    {
        let idx = &op.idx;
        if let Some((id, _, rec)) = idx.live_files().find(|(_, r, _)| *r == rel) {
            let defs = defs_of(idx, id);
            let imports: Vec<String> = idx
                .imports_of(id)
                .iter()
                .map(|i| idx.import_raw(id, i).to_string())
                .collect();
            return Ok(OutlineResult {
                rel,
                defs,
                imports,
                source: "index",
                lang,
                elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
                parse_errors: FileFlags(rec.flags).has(FileFlags::PARSE_ERRORS),
            });
        }
    }
    let src = greeg_lang::read_text(&path).with_context(|| format!("read {}", path.display()))?;
    if !lang.has_grammar() {
        bail!("{rel}: no grammar for this file type");
    }
    let ex = greeg_lang::sym::extract(lang, greeg_lang::sym::is_tsx(&rel), &src);
    let mut defs: Vec<DefSummary> = Vec::with_capacity(ex.symbols.len());
    for (i, s) in ex.symbols.iter().enumerate() {
        let mut chain = Vec::new();
        let mut cur = Some(i as u32);
        while let Some(c) = cur {
            let cs = &ex.symbols[c as usize];
            chain.push((cs.kind, ex.name(cs, &src).to_string()));
            cur = cs.parent;
        }
        chain.reverse();
        defs.push(DefSummary {
            name: ex.name(s, &src).to_string(),
            kind: s.kind,
            line: s.line,
            start: s.start,
            end: s.end,
            chain,
            flags: s.flags,
        });
    }
    let imports = ex.imports.iter().map(|i| i.module.clone()).collect();
    Ok(OutlineResult {
        rel,
        defs,
        imports,
        source: if ex.tree_sitter { "parse" } else { "regex" },
        lang,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
        parse_errors: ex.parse_errors,
    })
}

/// Lines of a definition body (or a window), as `show` and `def --mode block`
/// print them.
#[derive(Clone, Debug)]
pub struct Body {
    pub first: u32,
    pub lines: Vec<Vec<u8>>,
    /// The line the body (or window) runs to; past the last line shown when cut.
    pub last: u32,
    /// Cut by the token budget.
    pub clipped: bool,
}

/// Bodies longer than this are cut unless `--budget 0` asks for everything.
pub const BODY_CAP: u32 = 200;
/// Lines each way when no definition encloses the asked line.
pub const WINDOW: u32 = 20;

/// Read one file for bodies.
pub fn source_of(o: &Options, rel: &str) -> Result<crate::Source> {
    let path = o.root.join(rel);
    let bytes = greeg_lang::read_text(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(crate::Source::new(bytes))
}

/// Lines `from..=to`, capped at [`BODY_CAP`] (lifted by `--budget 0`) and
/// charged against `est` up to `o.budget`; at least three lines are kept.
pub fn body(o: &Options, src: &crate::Source, from: u32, to: u32, est: &mut usize) -> Body {
    let cap = if o.budget == 0 { u32::MAX } else { BODY_CAP };
    let from = from.max(1);
    // a trailing newline does not start a line (ripgrep prints nothing after it)
    let n = src.line_count();
    let n = if src.bytes.ends_with(b"\n") {
        n.saturating_sub(1)
    } else {
        n
    };
    let last = to.min(n).max(from);
    let stop = last.min(from.saturating_add(cap - 1));
    let (first, all) = src.lines(from, stop);
    let mut lines = Vec::with_capacity(all.len());
    let mut clipped = false;
    for (i, l) in all.into_iter().enumerate() {
        let cost = crate::tokens::code(&l) + 2;
        if o.budget > 0 && i >= 3 && *est + cost > o.budget {
            clipped = true;
            break;
        }
        *est += cost;
        lines.push(l);
    }
    Body {
        first,
        lines,
        last,
        clipped,
    }
}

/// One location's answer: the innermost definition enclosing the line, or
/// [`WINDOW`] lines each way when nothing does (a file without a grammar, a
/// line between definitions).
#[derive(Clone, Debug)]
pub struct ShowItem {
    pub rel: String,
    pub asked: u32,
    pub def: Option<DefSummary>,
    pub body: Body,
}

pub struct ShowResult {
    pub items: Vec<ShowItem>,
    pub source: &'static str,
    pub elapsed_ms: f64,
}

/// `greeg show FILE:LINE …`: the whole definition around a line, so a search
/// hit turns into one exact read instead of a guessed `sed -n` range.
pub fn show(o: &Options, locs: &[(String, u32)]) -> Result<ShowResult> {
    let t0 = Instant::now();
    let mut items = Vec::with_capacity(locs.len());
    let mut source = "text";
    let mut est = 0usize;
    for (file, asked) in locs {
        let rel = file.trim_start_matches("./").to_string();
        let src = source_of(o, &rel)?;
        let asked = (*asked).clamp(1, src.line_count().max(1));
        let defs = if Lang::from_path(&o.root.join(&rel)).has_grammar() {
            let ol = outline(o, &rel)?;
            source = ol.source;
            ol.defs
        } else {
            Vec::new()
        };
        // innermost: of the definitions enclosing the line, the one that starts last
        let mut best: Option<(DefSummary, u32)> = None;
        for d in defs {
            let end = src.end_line(d.start, d.end);
            if d.line <= asked
                && asked <= end
                && best.as_ref().is_none_or(|(b, _)| d.line >= b.line)
            {
                best = Some((d, end));
            }
        }
        let (def, from, to) = match best {
            Some((d, end)) => {
                let from = d.line;
                (Some(d), from, end)
            }
            None => (None, asked.saturating_sub(WINDOW).max(1), asked + WINDOW),
        };
        let body = body(o, &src, from, to, &mut est);
        items.push(ShowItem {
            rel,
            asked,
            def,
            body,
        });
    }
    Ok(ShowResult {
        items,
        source,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
    })
}

/// One file in a `map`.
#[derive(Clone, Debug)]
pub struct MapFile {
    pub rel: String,
    pub rank: f32,
    pub symbols: usize,
    pub by_kind: Vec<(DefKind, usize)>,
    /// Top exported symbols (kind, name).
    pub top: Vec<(DefKind, String)>,
    pub flags: FileFlags,
    pub imported_by: usize,
}

#[derive(Clone, Debug)]
pub struct MapDir {
    pub rel: String,
    pub files: usize,
    pub symbols: usize,
    pub rank: f32,
    pub top_files: Vec<String>,
}

pub struct MapResult {
    pub dir: String,
    pub files_total: usize,
    pub symbols_total: usize,
    pub dirs: Vec<MapDir>,
    pub files: Vec<MapFile>,
    pub elapsed_ms: f64,
    pub source: &'static str,
}

/// Count a file in its immediate subdirectory under `prefix`, if any.
fn count_in_dir(
    dirs: &mut BTreeMap<String, MapDir>,
    prefix: &str,
    rel: &str,
    symbols: usize,
    rank: f32,
) {
    let Some((sub, _)) = rel[prefix.len()..].split_once('/') else {
        return;
    };
    let d = dirs
        .entry(format!("{prefix}{sub}"))
        .or_insert_with(|| MapDir {
            rel: format!("{prefix}{sub}"),
            files: 0,
            symbols: 0,
            rank: 0.0,
            top_files: Vec::new(),
        });
    d.files += 1;
    d.symbols += symbols;
    d.rank = d.rank.max(rank);
    if d.top_files.len() < 3 {
        d.top_files.push(rel.to_string());
    }
}

/// `greeg map [DIR]`: the most important files and subdirectories by PageRank.
pub fn map(o: &Options, dir: &str) -> Result<MapResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    let dir = dir
        .trim_start_matches("./")
        .trim_end_matches('/')
        .trim_end_matches('.')
        .to_string();
    let Some(op) = (if o.use_index {
        indexed::open_fresh(o, threads)?
    } else {
        None
    }) else {
        bail!(
            "`map` needs the index (it is being built in the background; retry in a moment, or run `greeg index`)"
        );
    };
    let idx = &op.idx;
    if !idx.has_symbols() {
        bail!("`map` needs the symbol table; the index build is still in phase 1");
    }
    let sel = Selection::new(o, Vec::new())?;
    let Some(also) = sel.coverage(idx, op.pending.as_ref()) else {
        bail!(
            "`map` summarizes the index, which cannot cover this request: --hidden, --no-ignore, a glob that selects a hidden or ignored directory, or an index still recording what it skips (retry in a moment)"
        );
    };
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut files: Vec<MapFile> = Vec::new();
    let mut dirs: BTreeMap<String, MapDir> = BTreeMap::new();
    let mut symbols_total = 0usize;
    for (id, rel, rec) in idx.live_files() {
        let flags = FileFlags(rec.flags);
        if !rel.starts_with(&prefix) || !sel.selects(rel, flags) {
            continue;
        }
        let (first, syms) = idx
            .symbols_of(id)
            .unwrap_or((SymId { seg: 0, idx: 0 }, &[]));
        let n = syms.len();
        symbols_total += n;
        let rank = idx.rank(id);
        count_in_dir(&mut dirs, &prefix, rel, n, rank);
        if n == 0
            && !flags.has(FileFlags::PARSE_ERRORS)
            && !Lang::from_path(Path::new(rel)).has_grammar()
        {
            continue;
        }
        let mut by_kind: BTreeMap<u8, usize> = BTreeMap::new();
        let mut top: Vec<(f32, DefKind, String)> = Vec::new();
        for (i, s) in syms.iter().enumerate() {
            *by_kind.entry(s.kind).or_default() += 1;
            if s.parent == greeg_index::format::NONE && s.flags & SYM_TEST == 0 {
                let w = kind_weight(s.kind)
                    * if s.flags & SYM_EXPORTED != 0 {
                        1.0
                    } else {
                        0.7
                    };
                let name = idx
                    .sym_name(SymId {
                        seg: first.seg,
                        idx: first.idx + i as u32,
                    })
                    .to_string();
                if !top
                    .iter()
                    .any(|(_, k, n)| *k == kind_from_code(s.kind) && *n == name)
                {
                    top.push((w, kind_from_code(s.kind), name));
                }
            }
        }
        top.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
        });
        top.truncate(5);
        let imported_by = idx.in_edges(id).len();
        files.push(MapFile {
            rel: rel.to_string(),
            rank,
            symbols: n,
            by_kind: by_kind
                .into_iter()
                .map(|(k, c)| (kind_from_code(k), c))
                .collect(),
            top: top.into_iter().map(|(_, k, n)| (k, n)).collect(),
            flags,
            imported_by,
        });
    }
    // selected files the index skipped: no symbols or rank are known
    for rel in also.iter().filter(|r| r.starts_with(&prefix)) {
        count_in_dir(&mut dirs, &prefix, rel, 0, 0.0);
        if Lang::from_path(Path::new(rel)).has_grammar() {
            files.push(MapFile {
                rel: rel.clone(),
                rank: 0.0,
                symbols: 0,
                by_kind: Vec::new(),
                top: Vec::new(),
                flags: greeg_lang::path_flags(rel),
                imported_by: 0,
            });
        }
    }
    let files_total = files.len();
    files.sort_by(|a, b| {
        (b.rank * loc_w(b.flags, &b.rel, o.all))
            .partial_cmp(&(a.rank * loc_w(a.flags, &a.rel, o.all)))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rel.cmp(&b.rel))
    });
    let mut dirs: Vec<MapDir> = dirs.into_values().collect();
    dirs.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rel.cmp(&b.rel))
    });
    Ok(MapResult {
        dir,
        files_total,
        symbols_total,
        dirs,
        files,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
        source: "index",
    })
}

/// `greeg impact NAME`: what would break if NAME changed.
pub struct ImpactFile {
    pub rel: String,
    pub kinds: Vec<(HitKind, usize)>,
    pub flags: FileFlags,
    pub hits: usize,
    /// Sample lines (line, text).
    pub sample: Vec<(u32, String)>,
}

pub struct ImpactResult {
    /// Of the reference search the impact is read from.
    pub outcome: Outcome,
    pub name: String,
    pub defs: Vec<DefEntry>,
    pub will_break: Vec<ImpactFile>,
    pub may_break: Vec<ImpactFile>,
    pub review: Vec<ImpactFile>,
    pub callers: CallersResult,
    pub total_hits: usize,
    pub elapsed_ms: f64,
}

pub fn impact(o: &Options, name: &str) -> Result<ImpactResult> {
    let t0 = Instant::now();
    let mut so = o.clone();
    so.budget = 0;
    let r = refs(&so, name, &[])?;
    let (mut will, mut may, mut review) = (Vec::new(), Vec::new(), Vec::new());
    let mut total = 0usize;
    for f in &r.scan.files {
        let mut kinds: BTreeMap<HitKind, usize> = BTreeMap::new();
        for h in &f.hits {
            *kinds.entry(h.kind).or_default() += 1;
        }
        total += f.total;
        let is_test = f.flags.has(FileFlags::TEST);
        let strong = kinds.contains_key(&HitKind::Call)
            || kinds.contains_key(&HitKind::Type)
            || kinds.contains_key(&HitKind::Import);
        let weak = kinds.contains_key(&HitKind::Member) || kinds.contains_key(&HitKind::Ident);
        let mut sample: Vec<(u32, String)> = f
            .hits
            .iter()
            .filter(|h| !matches!(h.kind, HitKind::Comment | HitKind::Str | HitKind::Docstring))
            .take(3)
            .map(|h| (h.line, String::from_utf8_lossy(&h.text).to_string()))
            .collect();
        if sample.is_empty() {
            sample = f
                .hits
                .iter()
                .take(2)
                .map(|h| (h.line, String::from_utf8_lossy(&h.text).to_string()))
                .collect();
        }
        let entry = ImpactFile {
            rel: f.rel.clone(),
            kinds: kinds.into_iter().collect(),
            flags: f.flags,
            hits: f.total,
            sample,
        };
        if is_test || f.flags.demoted() || !f.lang.has_grammar() {
            review.push(entry);
        } else if strong {
            will.push(entry);
        } else if weak {
            may.push(entry);
        } else {
            review.push(entry);
        }
    }
    let mut co = o.clone();
    co.matching = crate::MatchingPolicy::Exact;
    co.fresh = greeg_index::fresh::Mode::None; // `refs` already ran the check
    let callers = callers(&co, name, 2)?;
    Ok(ImpactResult {
        outcome: Outcome::of_search(&r.scan, 0),
        name: name.to_string(),
        defs: r.defs,
        will_break: will,
        may_break: may,
        review,
        callers,
        total_hits: total,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use greeg_index::build::{BuildOpts, build};
    use greeg_index::fresh::Mode as Fresh;
    use std::fs;

    /// Module files follow symbols; generic file stems never match.
    #[test]
    fn def_lists_file_modules_after_symbols() {
        let base = std::env::temp_dir().join(format!("greeg-def-modules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        let dir = greeg_index::format_dir(&base.join("index"));
        fs::create_dir_all(root.join("src/net")).unwrap();
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "//! crate root\nmod sleep;\nmod net;\npub fn run() { sleep::sleep(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/sleep.rs"),
            "//! Sleep future.\npub fn sleep() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/net/mod.rs"), "pub fn connect() {}\n").unwrap();
        fs::write(
            root.join("pkg/__init__.py"),
            "\"\"\"The pkg package.\"\"\"\n",
        )
        .unwrap();
        // a file without a grammar is never a module
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/sleep.md"), "# sleep\n").unwrap();
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
        let o = Options {
            root: root.clone(),
            index_dir: Some(greeg_index::repo_of(&dir).to_path_buf()),
            fresh: Fresh::None,
            ..Default::default()
        };
        let d = |name: &str| def(&o, name, &[], None).unwrap();

        let r = d("sleep");
        assert_eq!(r.source, "index");
        let rows: Vec<(&str, u32, bool)> = r
            .entries
            .iter()
            .map(|e| (e.rel.as_str(), e.line, e.file_module))
            .collect();
        assert_eq!(
            rows,
            vec![("src/sleep.rs", 2, false), ("src/sleep.rs", 1, true)],
            "the symbol first, the file module last; `mod sleep;` is not a definition"
        );
        assert_eq!(r.total, 2);
        let m = &r.entries[1];
        assert_eq!(m.kind, DefKind::Module);
        assert_eq!(m.signature, "mod sleep");
        assert_eq!(m.doc.as_deref(), Some("Sleep future."));

        let r = d("net");
        let rows: Vec<(&str, bool)> = r
            .entries
            .iter()
            .map(|e| (e.rel.as_str(), e.file_module))
            .collect();
        assert_eq!(rows, vec![("src/net/mod.rs", true)]);

        let r = d("pkg");
        let rows: Vec<(&str, bool)> = r
            .entries
            .iter()
            .map(|e| (e.rel.as_str(), e.file_module))
            .collect();
        assert_eq!(rows, vec![("pkg/__init__.py", true)]);
        assert_eq!(r.entries[0].signature, "module pkg");
        assert_eq!(r.entries[0].doc.as_deref(), Some("The pkg package."));

        // a kind filter other than module drops the file entries
        let r = def(&o, "net", &[], Some(DefKind::Function)).unwrap();
        assert!(r.entries.iter().all(|e| !e.file_module));

        // generic stems and names with no module file
        for name in ["mod", "lib", "__init__", "connect"] {
            let r = d(name);
            assert!(
                r.entries.iter().all(|e| !e.file_module),
                "{name}: {:?}",
                r.entries.iter().map(|e| &e.rel).collect::<Vec<_>>()
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn show_prints_the_innermost_definition_or_a_window() {
        let base = std::env::temp_dir().join(format!("greeg-show-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("tree");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/a.rs"),
            "// top\npub struct S {\n    x: u32,\n}\nimpl S {\n    pub fn m(&self) -> u32 {\n        let y = self.x;\n        y + 1\n    }\n}\nfn tail() {}\n",
        )
        .unwrap();
        let long: String = (0..250).map(|i| format!("    let v{i} = {i};\n")).collect();
        fs::write(root.join("src/long.rs"), format!("fn big() {{\n{long}}}\n")).unwrap();
        fs::write(root.join("notes.md"), "line\n".repeat(50)).unwrap();
        let o = Options {
            root: root.clone(),
            use_index: false,
            ..Default::default()
        };
        let one = |file: &str, line: u32, o: &Options| {
            let r = show(o, &[(file.to_string(), line)]).unwrap();
            assert_eq!(r.items.len(), 1);
            r.items.into_iter().next().unwrap()
        };
        // inside a method: the method, not the impl block around it
        let it = one("src/a.rs", 7, &o);
        assert_eq!(it.def.as_ref().map(|d| d.name.as_str()), Some("m"));
        assert_eq!(
            (it.body.first, it.body.last, it.body.lines.len()),
            (6, 9, 4)
        );
        assert_eq!(it.body.lines[0], b"    pub fn m(&self) -> u32 {");
        assert!(!it.body.clipped);
        // a comment above everything: a window, clamped to the file's last line
        let it = one("src/a.rs", 1, &o);
        assert!(it.def.is_none());
        assert_eq!((it.body.first, it.body.last), (1, 11));
        // no grammar: a window around the line
        let it = one("notes.md", 30, &o);
        assert!(it.def.is_none());
        assert_eq!(
            (it.body.first, it.body.last, it.body.lines.len()),
            (10, 50, 41)
        );
        // the default budget cuts first; a large one reaches the 200-line cap; 0 lifts it
        let it = one("src/long.rs", 100, &o);
        assert_eq!(it.def.as_ref().map(|d| d.name.as_str()), Some("big"));
        assert!(it.body.clipped && it.body.lines.len() < 200 && it.body.last == 252);
        let it = one(
            "src/long.rs",
            100,
            &Options {
                budget: 100_000,
                ..o.clone()
            },
        );
        assert_eq!(
            (it.body.first, it.body.last, it.body.lines.len()),
            (1, 252, 200)
        );
        assert!(!it.body.clipped);
        let all = one(
            "src/long.rs",
            100,
            &Options {
                budget: 0,
                ..o.clone()
            },
        );
        assert_eq!(all.body.lines.len(), 252);
        // the budget keeps at least three lines and says so
        let tight = one(
            "src/long.rs",
            100,
            &Options {
                budget: 10,
                ..o.clone()
            },
        );
        assert_eq!(tight.body.lines.len(), 3);
        assert!(tight.body.clipped);
        // several locations share one budget
        let r = show(
            &Options {
                budget: 60,
                ..o.clone()
            },
            &[("src/a.rs".into(), 7), ("src/long.rs".into(), 5)],
        )
        .unwrap();
        assert_eq!(r.items.len(), 2);
        assert!(
            r.items[1].body.clipped,
            "the second body pays for the first"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
