//! `greeg find WORDS`: files ranked for a question in plain words, from the
//! index alone (ARCHITECTURE.md). Each query word is cut into identifier
//! pieces (`retry_backoff`, `RetryBackoff` → `retry`, `backoff`), and a file
//! scores BM25 over the pieces its words hold, plus bonuses for a definition
//! or a path segment holding the piece, times its location weight and
//! import rank. Weights were fixed on the discovery eval's development split.

use crate::indexed;
use crate::select::Selection;
use crate::{Options, loc_weight};
use anyhow::{Result, bail};
use greeg_index::index::SymId;
use greeg_index::symtab::{kind_from_code, kind_weight};
use greeg_lang::{DefKind, FileFlags};
use std::time::Instant;

const K1: f32 = 1.2;
const B: f32 = 0.75;
const W_DEF: f32 = 1.0;
const W_PATH: f32 = 3.0;
const W_RANK: f32 = 0.5;
/// Files kept with their definitions; the rest are only counted.
const MAX_FILES: usize = 50;
const DEFS_PER_FILE: usize = 3;
/// Words are at most 64 bytes, so are their pieces.
const MAX_PIECE: usize = greeg_index::words::MAX_WORD;

/// Words a question is made of rather than about.
const STOP: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "also", "an", "and", "any", "are",
    "as", "at", "be", "because", "been", "before", "being", "below", "between", "both", "but",
    "by", "can", "code", "could", "did", "do", "does", "doing", "down", "during", "each", "few",
    "file", "files", "for", "from", "function", "further", "get", "gets", "had", "has", "have",
    "having", "he", "her", "here", "his", "how", "i", "if", "in", "into", "is", "it", "its",
    "itself", "just", "me", "method", "more", "most", "my", "no", "nor", "not", "now", "of", "off",
    "on", "once", "only", "or", "other", "our", "out", "over", "own", "same", "set", "sets", "she",
    "should", "so", "some", "such", "than", "that", "the", "their", "them", "then", "there",
    "these", "they", "this", "those", "through", "to", "too", "under", "until", "up", "use",
    "used", "uses", "using", "very", "was", "way", "we", "were", "what", "when", "where", "which",
    "while", "who", "whom", "why", "will", "with", "would", "you", "your",
];

pub struct FindDef {
    pub kind: DefKind,
    pub name: String,
    pub line: u32,
}

pub struct FindFile {
    /// Root-relative path bytes (`greeg_index::rel`).
    pub rel: Vec<u8>,
    pub score: f32,
    pub flags: FileFlags,
    /// The query pieces the file holds, in query order.
    pub terms: Vec<String>,
    /// Its definitions whose names hold the most query pieces.
    pub defs: Vec<FindDef>,
}

pub struct FindResult {
    /// The pieces searched for, in query order.
    pub terms: Vec<String>,
    /// Pieces of the question no selected file holds.
    pub unknown: Vec<String>,
    pub files: Vec<FindFile>,
    /// Files holding at least one piece.
    pub files_total: usize,
    pub elapsed_ms: f64,
    pub source: &'static str,
    pub fresh: &'static str,
}

/// Call `f` on each identifier piece of `w` of 3+ bytes, cut as
/// `symtab::split_tokens` cuts names: at non-alphanumerics, before an
/// uppercase letter that follows a lowercase one or a digit, and before the
/// last capital of a run that a lowercase letter follows (`HTTPServer` →
/// `HTTP`, `Server`). Case is kept.
fn pieces(w: &[u8], mut f: impl FnMut(&[u8])) {
    let mut start = 0;
    for i in 0..=w.len() {
        let cut = match w.get(i) {
            None => true,
            Some(c) if !c.is_ascii_alphanumeric() => true,
            Some(c) if c.is_ascii_uppercase() && i > start => {
                let prev = w[i - 1];
                let next_lower = w.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                if prev.is_ascii_lowercase()
                    || prev.is_ascii_digit()
                    || (prev.is_ascii_uppercase() && next_lower)
                {
                    if i - start >= 3 {
                        f(&w[start..i]);
                    }
                    start = i;
                }
                continue;
            }
            Some(_) => continue,
        };
        if cut {
            if i - start >= 3 {
                f(&w[start..i]);
            }
            start = i + 1;
        }
    }
}

/// The question's pieces, lowercase, in order, without repeats or stop words.
pub fn query_terms(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = q.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !(b[i].is_ascii_alphabetic() || b[i] == b'_') {
            i += 1;
            continue;
        }
        let s = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let word = &q[s..i];
        if word.len() < 3 || STOP.contains(&word.to_ascii_lowercase().as_str()) {
            continue;
        }
        pieces(word.as_bytes(), |p| {
            let p = String::from_utf8_lossy(p).to_ascii_lowercase();
            if !STOP.contains(&p.as_str()) && !out.contains(&p) {
                out.push(p);
            }
        });
    }
    out
}

pub fn find(o: &Options, query: &str) -> Result<FindResult> {
    let t0 = Instant::now();
    let terms = query_terms(query);
    if terms.is_empty() {
        bail!(
            "`find` needs words of 3+ letters beyond common ones (a, the, how, file, function, …)"
        );
    }
    if !o.use_index {
        bail!("`find` needs the index");
    }
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    let op = indexed::open_for_graph(o, threads)?;
    let idx = &op.idx;
    if !idx.has_symbols() {
        bail!("`find` needs the symbol table; the index build is still in phase 1");
    }
    let sel = Selection::new(o, Vec::new())?;
    if sel.coverage(idx, op.pending.as_ref()).is_none() {
        bail!(
            "`find` ranks the index, which cannot cover this request: --hidden, --no-ignore, a glob that selects a hidden or ignored directory, or an index still recording what it skips (retry in a moment)"
        );
    }
    let term_of: hashbrown::HashMap<&[u8], usize> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_bytes(), i))
        .collect();
    // most pieces have no term's length: skip them before hashing
    let lengths = terms
        .iter()
        .filter(|t| t.len() <= MAX_PIECE)
        .fold(0u128, |m, t| m | 1 << t.len());
    let lookup = |p: &[u8], hit: &mut dyn FnMut(usize)| {
        if p.len() > MAX_PIECE || lengths & (1 << p.len()) == 0 {
            return;
        }
        let mut buf = [0u8; MAX_PIECE];
        let low = &mut buf[..p.len()];
        for (d, s) in low.iter_mut().zip(p) {
            *d = s.to_ascii_lowercase();
        }
        if let Some(&t) = term_of.get(&low[..]) {
            hit(t);
        }
    };
    let ids = idx
        .segments()
        .map(|(_, s)| (s.first_id + s.n_files) as usize)
        .max()
        .unwrap_or(0);

    // the selected files' sizes (NaN for the rest), and the pieces their paths hold
    let mut size = vec![f32::NAN; ids];
    let mut in_path: Vec<Vec<u32>> = vec![Vec::new(); terms.len()];
    let (mut n, mut total_size) = (0usize, 0f64);
    for (id, rel, rec) in idx.live_files() {
        let flags = FileFlags(rec.flags);
        if flags.has(FileFlags::BINARY) || !sel.selects(rel, flags) {
            continue;
        }
        let Some(s) = size.get_mut(id as usize) else {
            continue;
        };
        *s = rec.size as f32;
        n += 1;
        total_size += rec.size as f64;
        pieces(rel, |p| {
            lookup(p, &mut |t| {
                if in_path[t].last() != Some(&id) {
                    in_path[t].push(id)
                }
            })
        });
    }
    let selected = |id: u32| size.get(id as usize).is_some_and(|s| !s.is_nan());
    let avg = (total_size / n.max(1) as f64).max(1.0) as f32;

    // per piece, a file's id once for each of its distinct words holding it
    let mut in_words: Vec<Vec<u32>> = vec![Vec::new(); terms.len()];
    idx.scan_words(
        |w| {
            let mut hit: Vec<usize> = Vec::new();
            pieces(w, |p| {
                lookup(p, &mut |t| {
                    if !hit.contains(&t) {
                        hit.push(t)
                    }
                })
            });
            (!hit.is_empty()).then_some(hit)
        },
        |hit, files| {
            for id in files.filter(|&id| selected(id)) {
                for &t in &hit {
                    in_words[t].push(id);
                }
            }
        },
    );

    // files defining a name that holds the piece
    let mut in_def: Vec<Vec<u32>> = vec![Vec::new(); terms.len()];
    for (_, seg) in idx.segments() {
        let Some(sv) = seg.symbols() else { continue };
        for (t, term) in terms.iter().enumerate() {
            let whole = sv.find_name(term);
            let parts = sv.find_token(term).unwrap_or_default();
            for &nid in whole.iter().chain(parts) {
                for &s in sv.syms_named(nid) {
                    let Some(r) = sv.sym(s) else { continue };
                    let id = seg.first_id + r.file;
                    if selected(id) {
                        in_def[t].push(id);
                    }
                }
            }
        }
    }

    // BM25 per piece over dense per-file tables, reset after each piece
    const PATH: u8 = 1;
    const DEF: u8 = 2;
    let mut tf = vec![0u16; ids];
    let mut marks = vec![0u8; ids];
    let mut score = vec![0f32; ids];
    let mut scored: Vec<u32> = Vec::new();
    let mut holders_of: Vec<Vec<u32>> = Vec::with_capacity(terms.len());
    let mut unknown = Vec::new();
    for (t, term) in terms.iter().enumerate() {
        let mut holders: Vec<u32> = Vec::new();
        for &id in &in_words[t] {
            let f = &mut tf[id as usize];
            if *f == 0 && marks[id as usize] == 0 {
                holders.push(id);
            }
            *f = f.saturating_add(1);
        }
        for (list, bit) in [(&in_path[t], PATH), (&in_def[t], DEF)] {
            for &id in list {
                let i = id as usize;
                if tf[i] == 0 && marks[i] == 0 {
                    holders.push(id);
                }
                marks[i] |= bit;
            }
        }
        if holders.is_empty() {
            unknown.push(term.clone());
        }
        let df = holders.len() as f32;
        let idf = (1.0 + (n as f32 - df + 0.5) / (df + 0.5)).ln();
        for &id in &holders {
            let i = id as usize;
            let f = tf[i] as f32;
            let body = f * (K1 + 1.0) / (f + K1 * (1.0 - B + B * size[i] / avg));
            let path = if marks[i] & PATH != 0 { W_PATH } else { 0.0 };
            let def = if marks[i] & DEF != 0 { W_DEF } else { 0.0 };
            if score[i] == 0.0 {
                scored.push(id);
            }
            score[i] += idf * (body + path + def);
            tf[i] = 0;
            marks[i] = 0;
        }
        holders.sort_unstable();
        holders_of.push(holders);
    }
    let prior = |id: u32| {
        let Some(rec) = idx.rec(id) else { return 0.0 };
        let rel = idx.path(id).unwrap_or_default();
        let rank = if rec.rank > 0 {
            (rec.rank - 1) as f32 / 65534.0
        } else {
            0.0
        };
        loc_weight(FileFlags(rec.flags), &greeg_index::rel::display(rel), o.all)
            * (1.0 + W_RANK * rank.sqrt())
    };
    let mut ranked: Vec<(u32, f32)> = scored
        .into_iter()
        .map(|id| (id, score[id as usize] * prior(id)))
        .collect();
    let files_total = ranked.len();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| idx.path(a.0).cmp(&idx.path(b.0)))
    });
    ranked.truncate(MAX_FILES);

    let files = ranked
        .into_iter()
        .filter_map(|(id, score)| {
            let rel = idx.path(id)?.to_vec();
            let flags = FileFlags(idx.rec(id)?.flags);
            let held = (0..terms.len())
                .filter(|&t| holders_of[t].binary_search(&id).is_ok())
                .map(|t| terms[t].clone())
                .collect();
            Some(FindFile {
                defs: best_defs(idx, id, &lookup),
                rel,
                score,
                flags,
                terms: held,
            })
        })
        .collect();
    drop(term_of);
    if let Some(rb) = indexed::failed_check(o, idx) {
        return Err(rb.into());
    }
    Ok(FindResult {
        terms,
        unknown,
        files,
        files_total,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
        source: "index",
        fresh: op.fresh_method,
    })
}

/// Calls back with the term index of a piece, if it is one of the question's.
type Lookup<'a> = dyn Fn(&[u8], &mut dyn FnMut(usize)) + 'a;

/// A file's definitions whose names hold the most distinct query pieces,
/// weighted by kind; ties go to the earlier line.
fn best_defs(idx: &greeg_index::Index, id: u32, lookup: &Lookup) -> Vec<FindDef> {
    let Some((first, syms)) = idx.symbols_of(id) else {
        return Vec::new();
    };
    let mut best: Vec<(f32, u32, DefKind, String)> = Vec::new();
    for (i, s) in syms.iter().enumerate() {
        let name = idx.sym_name(SymId {
            seg: first.seg,
            idx: first.idx + i as u32,
        });
        let mut held: Vec<usize> = Vec::new();
        pieces(name.as_bytes(), |p| {
            lookup(p, &mut |t| {
                if !held.contains(&t) {
                    held.push(t)
                }
            })
        });
        if held.is_empty() {
            continue;
        }
        let kind = kind_from_code(s.kind);
        if best.iter().any(|b| b.2 == kind && b.3 == name) {
            continue;
        }
        best.push((
            held.len() as f32 * kind_weight(s.kind),
            s.line,
            kind,
            name.to_string(),
        ));
    }
    best.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    best.truncate(DEFS_PER_FILE);
    best.into_iter()
        .map(|(_, line, kind, name)| FindDef { kind, name, line })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(w: &str) -> Vec<String> {
        let mut v = Vec::new();
        pieces(w.as_bytes(), |p| {
            v.push(String::from_utf8_lossy(p).into_owned())
        });
        v
    }

    #[test]
    fn pieces_cut_identifiers_as_name_tokens_are_cut() {
        assert_eq!(cut("retry_backoff"), ["retry", "backoff"]);
        assert_eq!(cut("RetryBackoff"), ["Retry", "Backoff"]);
        assert_eq!(cut("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(cut("parseHttp2Frame"), ["parse", "Http2", "Frame"]);
        assert_eq!(cut("io_uring"), ["uring"]);
        assert_eq!(
            cut("src/net/tcp_stream.rs"),
            ["src", "net", "tcp", "stream"]
        );
        for w in [
            "retry_backoff",
            "RetryBackoff",
            "HTTPServer",
            "parseHttp2Frame",
            "XMLHttpRequest",
        ] {
            let lower: Vec<String> = cut(w).iter().map(|p| p.to_ascii_lowercase()).collect();
            assert_eq!(lower, greeg_index::symtab::split_tokens(w), "{w}");
        }
    }

    #[test]
    fn a_question_keeps_its_content_words_once() {
        assert_eq!(
            query_terms("Where is the retry backoff computed? RetryBackoff, retry_policy"),
            ["retry", "backoff", "computed", "policy"]
        );
        assert!(query_terms("how does it do that").is_empty());
    }
}
