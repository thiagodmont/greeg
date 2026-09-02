//! Symbol verbs (DESIGN.md §7.3–§7.4): `def`, `refs`, `callers`, `impls`,
//! `outline`, `map`, `impact`. Each verb returns plain data; rendering lives
//! in the CLI. All verbs work without an index (slower, via scan mode) except
//! `map`, which needs the symbol table.

use crate::indexed::{self, defs_of};
use crate::{DefSummary, HitKind, Mode, Options, Rung, ScanResult, scan};
use anyhow::{Context, Result, bail};
use greeg_index::Index;
use greeg_index::index::SymId;
use greeg_index::symtab::{kind_from_code, kind_weight};
use greeg_lang::sym::{SYM_EXPORTED, SYM_HAS_DOC, SYM_TEST};
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

fn loc_w(flags: FileFlags, all: bool) -> f32 {
    if all {
        1.0
    } else if flags.has(FileFlags::MINIFIED) {
        0.1
    } else if flags.has(FileFlags::GENERATED | FileFlags::VENDORED | FileFlags::LOCKFILE) {
        0.2
    } else if flags.has(FileFlags::TEST) {
        0.45
    } else {
        1.0
    }
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Import reachability (DESIGN.md §7.3): 1.0 direct, 0.8 within two hops,
/// 0.6 same directory, 0.4 otherwise. `origins` are file ids.
pub fn reach(idx: &Index, origins: &[u32], target: u32) -> f32 {
    if origins.is_empty() {
        return 0.6;
    }
    let trel = idx.path(target).unwrap_or("");
    let mut best = 0.4f32;
    for &o in origins {
        if o == target {
            return 1.0;
        }
        if let Some(g) = idx.graph() {
            let out = g.out(o);
            if out.contains(&target) {
                return 1.0;
            }
            for &mid in out.iter().take(64) {
                if g.out(mid).contains(&target) {
                    best = best.max(0.8);
                    break;
                }
            }
            // reverse direction: the target imports the origin (siblings that share code)
            if g.incoming(o).contains(&target) {
                best = best.max(0.8);
            }
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

/// Signature line and doc first line for a definition at `start` in `src`.
fn signature_and_doc(idx: Option<(&Index, u32)>, src: &[u8], start: u32, flags: u8, lang: Lang) -> (String, Option<String>) {
    let s = start as usize;
    let le = memchr::memchr(b'\n', &src[s.min(src.len())..]).map(|k| s + k).unwrap_or(src.len());
    let sig = String::from_utf8_lossy(src[s..le].trim_ascii()).to_string();
    let sig = if sig.len() > 200 { format!("{}…", &sig[..sig.char_indices().nth(197).map(|(i, _)| i).unwrap_or(197)]) } else { sig };
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
                            Some((k2, a2, _)) if (k2 == 0 || k2 == 2) && src[q as usize..p as usize].iter().filter(|&&x| x == b'\n').count() <= 1 => {
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
            if !(c.is_ascii_whitespace() || c == b'#' || c == b'@' || c == b'[' || c == b']' || c.is_ascii_alphanumeric() || c == b'_' || c == b'(' || c == b')' || c == b'.' || c == b'=' || c == b'"' || c == b',') {
                break;
            }
            probe -= 1;
            n += 1;
        }
        if lang == Lang::Python {
            let body = &src[s..src.len().min(s + 4096)];
            if let Some(q) = memchr::memmem::find(body, b"\"\"\"").or_else(|| memchr::memmem::find(body, b"'''")) {
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
        let l = l.trim().trim_start_matches("/**").trim_start_matches("/*!").trim_start_matches("///").trim_start_matches("//!").trim_start_matches("/*").trim_start_matches('*').trim_start_matches('#').trim_start_matches("\"\"\"").trim_start_matches("'''").trim_start_matches("r\"\"\"").trim().trim_end_matches("*/").trim_end_matches("\"\"\"").trim_end_matches("'''").trim();
        if !l.is_empty() {
            return l.chars().take(120).collect();
        }
    }
    String::new()
}

fn kind_filter_ok(kinds: &[HitKind], _k: DefKind) -> bool {
    kinds.is_empty() || kinds.contains(&HitKind::Def)
}

/// `greeg def NAME`.
pub fn def(o: &Options, name: &str, from: &[String], want_kind: Option<DefKind>) -> Result<DefResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 { crate::default_threads() } else { o.threads };
    let mut res = DefResult { name: name.to_string(), ..Default::default() };
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
    {
        let idx = &op.idx;
        res.source = "index";
        res.fresh = op.fresh_method;
        let mut syms = idx.lookup(name);
        let mut rung = Rung::Exact;
        if syms.is_empty() && o.ladder {
            // ladder over names: case-insensitive, split tokens, fuzzy
            let lower = name.to_lowercase();
            let mut alts: Vec<String> = Vec::new();
            for (_, seg) in idx.segments() {
                if let Some(sv) = seg.symbols() {
                    // case-insensitive: scan the prefix range of the first char in both cases
                    for first in [lower.chars().next().unwrap_or('a').to_ascii_lowercase(), lower.chars().next().unwrap_or('a').to_ascii_uppercase()] {
                        let r = sv.prefix_range(&first.to_string());
                        for nid in r {
                            let n = sv.name(nid);
                            if n.len() == name.len() && n.eq_ignore_ascii_case(name) && !alts.contains(&n.to_string()) {
                                alts.push(n.to_string());
                            }
                        }
                    }
                }
            }
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
                    alts = idx.fuzzy_names(name, d, 8).into_iter().map(|(n, _)| n).collect();
                    if !alts.is_empty() {
                        rung = Rung::Fuzzy(alts.clone());
                    }
                }
            }
            for a in &alts {
                syms.extend(idx.lookup(a));
            }
            res.suggestions = alts;
        }
        res.rung = rung;
        res.total = syms.len();
        let origins = origin_ids(idx, from);
        let mut entries: Vec<DefEntry> = Vec::with_capacity(syms.len().min(64));
        for s in syms.iter().take(256) {
            let Some(r) = idx.sym(*s) else { continue };
            let kind = kind_from_code(r.kind);
            if let Some(wk) = want_kind
                && wk != kind
            {
                continue;
            }
            let fid = idx.sym_file(*s);
            let rec = idx.rec(fid).context("file record")?;
            let fflags = FileFlags(rec.flags);
            let rel = idx.path(fid).unwrap_or("").to_string();
            let rch = reach(idx, &origins, fid);
            let kw = kind_weight(r.kind);
            let exported = if r.flags & SYM_EXPORTED != 0 { 1.0 } else { 0.85 };
            let score = kw * exported * loc_w(fflags, o.all) * (0.6 + 0.4 * idx.rank(fid)) * rch;
            let chain: Vec<(DefKind, String)> = idx.sym_chain(*s).into_iter().map(|(k, n)| (kind_from_code(k), n.to_string())).collect();
            let chain = chain[..chain.len().saturating_sub(1)].to_vec();
            entries.push(DefEntry { rel, line: r.line, kind, name: idx.sym_name(*s).to_string(), chain, signature: String::new(), doc: None, flags: r.flags, file_flags: fflags, supers: idx.sym_supers(*s).iter().map(|s| s.to_string()).collect(), score, reach: rch, start: r.start, end: r.end, file_id: Some(fid) });
        }
        entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)).then(a.line.cmp(&b.line)));
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
        let show = if o.budget == 0 { entries.len() } else { (o.budget / 40).clamp(3, 40) };
        let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for e in entries.iter_mut().take(show) {
            let src = cache.entry(e.rel.clone()).or_insert_with(|| greeg_lang::read_text(o.root.join(&e.rel)).unwrap_or_default());
            if src.is_empty() || e.start as usize >= src.len() {
                continue;
            }
            let (sig, doc) = signature_and_doc(e.file_id.map(|id| (idx, id)), src, e.start, e.flags, Lang::from_path(Path::new(&e.rel)));
            e.signature = sig;
            e.doc = doc;
        }
        entries.truncate(show.max(1));
        res.entries = entries;
        res.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
        return Ok(res);
    }
    // scan fallback: definition hits by regex
    let mut so = o.clone();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = vec![HitKind::Def];
    so.mode = Mode::Content;
    so.budget = 0;
    let mut r = scan(&so)?;
    let idxs: Vec<usize> = (0..r.files.len().min(200)).collect();
    crate::refine(&mut r, &idxs);
    res.source = if r.stats.source == "index" { "index (phase 1)" } else { "scan" };
    res.rung = r.rung.clone();
    let mut entries = Vec::new();
    for f in &r.files {
        for h in &f.hits {
            if h.kind != HitKind::Def {
                continue;
            }
            let (kind, chain, dstart, dend) = match h.def_idx.map(|d| &f.defs[d as usize]) {
                Some(d) => (d.kind, h.chain[..h.chain.len().saturating_sub(1)].to_vec(), d.start, d.end),
                None => (DefKind::Function, h.chain.clone(), h.line_start, h.match_end),
            };
            if let Some(wk) = want_kind
                && wk != kind
            {
                continue;
            }
            let _ = kind_filter_ok(&so.kinds, kind);
            entries.push(DefEntry { rel: f.rel.clone(), line: h.line, kind, name: name.to_string(), chain, signature: String::from_utf8_lossy(&h.text).to_string(), doc: None, flags: 0, file_flags: f.flags, supers: vec![], score: h.score, reach: 0.6, start: dstart, end: dend, file_id: None });
        }
    }
    res.total = entries.len();
    entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
    let show = if o.budget == 0 { entries.len() } else { (o.budget / 40).clamp(3, 40) };
    entries.truncate(show.max(1));
    res.entries = entries;
    res.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(res)
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
    let mut d = o.clone();
    d.ladder = false;
    d.budget = 400;
    let defs = def(&d, name, &[], None).map(|r| r.entries).unwrap_or_default();
    let (mut resolved, mut classified) = (0usize, 0usize);
    if let Some(op) = if o.use_index { indexed::open_fresh(o, 1).ok().flatten() } else { None }
        && op.idx.has_symbols()
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
    Ok(RefsResult { scan: scan_r, defs, resolved, classified })
}

/// One calling function (DESIGN.md §7.4 `callers`).
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
                Some(d) => (if d.chain.is_empty() { h.chain.clone() } else { d.chain.clone() }, d.line),
                None => (Vec::new(), h.line),
            };
            let e = map.entry((f.rel.clone(), h.def_idx)).or_insert_with(|| Caller { rel: f.rel.clone(), chain, def_line, count: 0, lines: Vec::new(), file_flags: f.flags, score: f.prior, called_by: Vec::new() });
            e.count += 1;
            if e.lines.len() < 8 {
                e.lines.push(h.line);
            }
        }
    }
    let mut callers: Vec<Caller> = map.into_values().collect();
    callers.sort_by(|a, b| (b.score * (1.0 + (b.count as f32).ln())).partial_cmp(&(a.score * (1.0 + (a.count as f32).ln()))).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
    if depth >= 2 {
        let mut seen: Vec<String> = Vec::new();
        for c in callers.iter_mut().take(12) {
            let Some((_, cname)) = c.chain.last() else { continue };
            if cname == name || seen.contains(cname) {
                continue;
            }
            seen.push(cname.clone());
            let mut so2 = so.clone();
            so2.pattern = cname.clone();
            so2.ladder = false;
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
    Ok(CallersResult { name: name.to_string(), files: r.files.len(), callers, total_hits: total, source: r.stats.source, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3, rung: r.rung.clone() })
}

pub struct ImplsResult {
    pub name: String,
    /// From the symbol table's supertype lists.
    pub direct: Vec<DefEntry>,
    /// Type-position hits on definition lines that the resolver did not tie to a supertype list.
    pub extras: Vec<DefEntry>,
    pub source: &'static str,
    pub elapsed_ms: f64,
}

pub fn impls(o: &Options, name: &str) -> Result<ImplsResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 { crate::default_threads() } else { o.threads };
    let mut direct = Vec::new();
    let mut source = "scan";
    let mut have: Vec<(String, u32)> = Vec::new();
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
    {
        let idx = &op.idx;
        source = "index";
        let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for s in idx.implementors(name).into_iter().take(200) {
            let Some(r) = idx.sym(s) else { continue };
            let fid = idx.sym_file(s);
            let rel = idx.path(fid).unwrap_or("").to_string();
            let fflags = FileFlags(idx.rec(fid).map(|r| r.flags).unwrap_or(0));
            let chain: Vec<(DefKind, String)> = idx.sym_chain(s).into_iter().map(|(k, n)| (kind_from_code(k), n.to_string())).collect();
            let chain = chain[..chain.len().saturating_sub(1)].to_vec();
            let src = cache.entry(rel.clone()).or_insert_with(|| greeg_lang::read_text(o.root.join(&rel)).unwrap_or_default());
            let (sig, doc) = if (r.start as usize) < src.len() { signature_and_doc(Some((idx, fid)), src, r.start, r.flags, Lang::from_path(Path::new(&rel))) } else { (String::new(), None) };
            let score = kind_weight(r.kind) * loc_w(fflags, o.all) * (0.6 + 0.4 * idx.rank(fid));
            have.push((rel.clone(), r.line));
            direct.push(DefEntry { rel, line: r.line, kind: kind_from_code(r.kind), name: idx.sym_name(s).to_string(), chain, signature: sig, doc, flags: r.flags, file_flags: fflags, supers: idx.sym_supers(s).iter().map(|x| x.to_string()).collect(), score, reach: 0.6, start: r.start, end: r.end, file_id: Some(fid) });
        }
        direct.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
    }
    // extras: type-position hits on definition lines
    let mut so = o.clone();
    so.pattern = name.to_string();
    so.fixed_strings = true;
    so.word = true;
    so.kinds = vec![HitKind::Type];
    so.mode = Mode::Content;
    so.budget = 0;
    so.ladder = false;
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
                let Some((ns, ne)) = greeg_lang::defs::def_name_on_line(lang, line) else { continue };
                let dname = String::from_utf8_lossy(&line[ns..ne]).to_string();
                if dname == name || have.contains(&(f.rel.clone(), h.line)) {
                    continue;
                }
                let kind = h.def_idx.map(|d| f.defs[d as usize].kind).unwrap_or(DefKind::Class);
                if !kind.is_container() && kind != DefKind::TypeAlias {
                    continue;
                }
                extras.push(DefEntry { rel: f.rel.clone(), line: h.line, kind, name: dname, chain: h.chain[..h.chain.len().saturating_sub(1)].to_vec(), signature: String::from_utf8_lossy(line).to_string(), doc: None, flags: 0, file_flags: f.flags, supers: vec![name.to_string()], score: h.score, reach: 0.6, start: h.line_start, end: h.match_end, file_id: f.file_id });
            }
        }
        extras.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
        extras.truncate(40);
    }
    Ok(ImplsResult { name: name.to_string(), direct, extras, source, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3 })
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
    let threads = if o.threads == 0 { crate::default_threads() } else { o.threads };
    if o.use_index
        && let Some(op) = indexed::open_fresh(o, threads)?
        && op.idx.has_symbols()
    {
        let idx = &op.idx;
        if let Some((id, _, rec)) = idx.live_files().find(|(_, r, _)| *r == rel) {
            let defs = defs_of(idx, id);
            let imports: Vec<String> = idx.imports_of(id).iter().map(|i| idx.import_raw(id, i).to_string()).collect();
            return Ok(OutlineResult { rel, defs, imports, source: "index", lang, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3, parse_errors: FileFlags(rec.flags).has(FileFlags::PARSE_ERRORS) });
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
        defs.push(DefSummary { name: ex.name(s, &src).to_string(), kind: s.kind, line: s.line, start: s.start, end: s.end, chain, flags: s.flags });
    }
    let imports = ex.imports.iter().map(|i| i.module.clone()).collect();
    Ok(OutlineResult { rel, defs, imports, source: if ex.tree_sitter { "parse" } else { "regex" }, lang, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3, parse_errors: ex.parse_errors })
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

/// `greeg map [DIR]`: the most important files and subdirectories by PageRank.
pub fn map(o: &Options, dir: &str) -> Result<MapResult> {
    let t0 = Instant::now();
    let threads = if o.threads == 0 { crate::default_threads() } else { o.threads };
    let dir = dir.trim_start_matches("./").trim_end_matches('/').to_string();
    let Some(op) = (if o.use_index { indexed::open_fresh(o, threads)? } else { None }) else {
        bail!("`map` needs the index (it is being built in the background; retry in a moment, or run `greeg index`)");
    };
    let idx = &op.idx;
    if !idx.has_symbols() {
        bail!("`map` needs the symbol table; the index build is still in phase 1");
    }
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut files: Vec<MapFile> = Vec::new();
    let mut dirs: BTreeMap<String, MapDir> = BTreeMap::new();
    let mut symbols_total = 0usize;
    for (id, rel, rec) in idx.live_files() {
        if !rel.starts_with(&prefix) {
            continue;
        }
        let flags = FileFlags(rec.flags);
        let (first, syms) = idx.symbols_of(id).unwrap_or((SymId { seg: 0, idx: 0 }, &[]));
        let n = syms.len();
        symbols_total += n;
        let rank = idx.rank(id);
        // immediate subdirectory under `dir`
        let rest = &rel[prefix.len()..];
        if let Some((sub, _)) = rest.split_once('/') {
            let d = dirs.entry(format!("{prefix}{sub}")).or_insert_with(|| MapDir { rel: format!("{prefix}{sub}"), files: 0, symbols: 0, rank: 0.0, top_files: Vec::new() });
            d.files += 1;
            d.symbols += n;
            d.rank = d.rank.max(rank);
            if d.top_files.len() < 3 {
                d.top_files.push(rel.to_string());
            }
        }
        if n == 0 && !flags.has(FileFlags::PARSE_ERRORS) && !Lang::from_path(Path::new(rel)).has_grammar() {
            continue;
        }
        let mut by_kind: BTreeMap<u8, usize> = BTreeMap::new();
        let mut top: Vec<(f32, DefKind, String)> = Vec::new();
        for (i, s) in syms.iter().enumerate() {
            *by_kind.entry(s.kind).or_default() += 1;
            if s.parent == greeg_index::format::NONE && s.flags & SYM_TEST == 0 {
                let w = kind_weight(s.kind) * if s.flags & SYM_EXPORTED != 0 { 1.0 } else { 0.7 };
                let name = idx.sym_name(SymId { seg: first.seg, idx: first.idx + i as u32 }).to_string();
                if !top.iter().any(|(_, k, n)| *k == kind_from_code(s.kind) && *n == name) {
                    top.push((w, kind_from_code(s.kind), name));
                }
            }
        }
        top.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.2.cmp(&b.2)));
        top.truncate(5);
        let imported_by = idx.graph().map(|g| g.incoming(id).len()).unwrap_or(0);
        files.push(MapFile { rel: rel.to_string(), rank, symbols: n, by_kind: by_kind.into_iter().map(|(k, c)| (kind_from_code(k), c)).collect(), top: top.into_iter().map(|(_, k, n)| (k, n)).collect(), flags, imported_by });
    }
    let files_total = files.len();
    files.sort_by(|a, b| (b.rank * loc_w(b.flags, o.all)).partial_cmp(&(a.rank * loc_w(a.flags, o.all))).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
    let mut dirs: Vec<MapDir> = dirs.into_values().collect();
    dirs.sort_by(|a, b| b.rank.partial_cmp(&a.rank).unwrap_or(std::cmp::Ordering::Equal).then(a.rel.cmp(&b.rel)));
    Ok(MapResult { dir, files_total, symbols_total, dirs, files, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3, source: "index" })
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
        let strong = kinds.contains_key(&HitKind::Call) || kinds.contains_key(&HitKind::Type) || kinds.contains_key(&HitKind::Import);
        let weak = kinds.contains_key(&HitKind::Member) || kinds.contains_key(&HitKind::Ident);
        let mut sample: Vec<(u32, String)> = f.hits.iter().filter(|h| !matches!(h.kind, HitKind::Comment | HitKind::Str | HitKind::Docstring)).take(3).map(|h| (h.line, String::from_utf8_lossy(&h.text).to_string())).collect();
        if sample.is_empty() {
            sample = f.hits.iter().take(2).map(|h| (h.line, String::from_utf8_lossy(&h.text).to_string())).collect();
        }
        let entry = ImpactFile { rel: f.rel.clone(), kinds: kinds.into_iter().collect(), flags: f.flags, hits: f.total, sample };
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
    co.ladder = false;
    let callers = callers(&co, name, 2)?;
    Ok(ImpactResult { name: name.to_string(), defs: r.defs, will_break: will, may_break: may, review, callers, total_hits: total, elapsed_ms: t0.elapsed().as_secs_f64() * 1e3 })
}
