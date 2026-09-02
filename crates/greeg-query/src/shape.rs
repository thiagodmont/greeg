//! Shape a `ScanResult` to a budget: choose content vs facets, rank, cap per
//! file, add adaptive context, and compute the footer (DESIGN.md §6.4, §10).

use crate::{FileResult, HitKind, Mode, Rung, ScanResult, tokens};
use greeg_lang::{DefKind, FileFlags};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Files,
    Count,
    Outline,
    Content,
    Block,
    Facets,
}

#[derive(Clone, Debug)]
pub struct ShownHit {
    pub hit: usize,
    /// (first line number, lines) when context is attached.
    pub context: Option<(u32, Vec<Vec<u8>>)>,
    /// Whole enclosing block (block layout).
    pub block: Option<(u32, Vec<Vec<u8>>)>,
    /// Context was already shown earlier in this session (DESIGN.md §9 dedup).
    pub seen_before: bool,
}

#[derive(Clone, Debug)]
pub struct ShownFile {
    pub file: usize,
    pub hits: Vec<ShownHit>,
    pub more: usize,
    pub best: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Facets {
    pub by_kind: Vec<(HitKind, usize)>,
    pub by_dir: Vec<(String, usize)>,
    pub by_lang: Vec<(String, usize)>,
    pub by_flag: Vec<(&'static str, usize)>,
    pub word_hits: usize,
    /// (file index, hit index) of definition hits, best first.
    pub top_defs: Vec<(usize, usize)>,
    pub top_hits: Vec<(usize, usize)>,
    pub defs_total: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Footer {
    pub hits_shown: usize,
    pub hits_total: usize,
    pub files_shown: usize,
    pub files_total: usize,
    pub demoted_files: usize,
    pub demoted_hits: usize,
    pub minified_hits: usize,
    pub skipped_binary: usize,
    pub skipped_huge: usize,
    pub rung: Rung,
    pub ignored_only: Option<(usize, usize)>,
    pub est_tokens: usize,
    pub elapsed_ms: f64,
    pub hints: Vec<String>,
}

#[derive(Debug)]
pub struct Report {
    pub layout: Layout,
    pub files: Vec<ShownFile>,
    pub facets: Option<Facets>,
    pub footer: Footer,
}

fn kind_counts(r: &ScanResult) -> Vec<(HitKind, usize)> {
    HitKind::ALL.iter().map(|k| (*k, r.stats.by_kind[k.idx()])).filter(|(_, n)| *n > 0).collect()
}

fn top_dir(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.len() {
        0 | 1 => "(root)".to_string(),
        2 => parts[0].to_string(),
        _ => format!("{}/{}", parts[0], parts[1]),
    }
}

fn facets(r: &ScanResult, ranked: &[(usize, usize, f32)]) -> Facets {
    let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_flag: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut word_hits = 0usize;
    for f in &r.files {
        *by_dir.entry(top_dir(&f.rel)).or_default() += f.total;
        *by_lang.entry(f.lang.short().to_string()).or_default() += f.total;
        for n in f.flags.names() {
            *by_flag.entry(n).or_default() += f.total;
        }
        if !f.flags.demoted() {
            *by_flag.entry("source").or_default() += f.total;
        }
        for h in &f.hits {
            let pat = r.opts.pattern.as_bytes();
            let m = h.match_end - h.match_start;
            if m as usize == pat.len() {
                word_hits += 1;
            }
        }
    }
    let mut by_dir: Vec<_> = by_dir.into_iter().collect();
    by_dir.sort_by(|a, b| b.1.cmp(&a.1));
    by_dir.truncate(8);
    let mut by_lang: Vec<_> = by_lang.into_iter().collect();
    by_lang.sort_by(|a, b| b.1.cmp(&a.1));
    let mut by_flag: Vec<_> = by_flag.into_iter().collect();
    by_flag.sort_by(|a, b| b.1.cmp(&a.1));
    let top_defs: Vec<(usize, usize)> = ranked.iter().filter(|(fi, hi, _)| r.files[*fi].hits[*hi].kind == HitKind::Def).map(|(fi, hi, _)| (*fi, *hi)).collect();
    let defs_total = top_defs.len();
    let top_hits: Vec<(usize, usize)> = ranked.iter().filter(|(fi, hi, _)| r.files[*fi].hits[*hi].kind != HitKind::Def).map(|(fi, hi, _)| (*fi, *hi)).take(12).collect();
    Facets { by_kind: kind_counts(r), by_dir, by_lang, by_flag, word_hits, top_defs, top_hits, defs_total }
}

fn read_lines(path: &std::path::Path, from_line: u32, to_line: u32) -> Option<(u32, Vec<Vec<u8>>)> {
    let src = greeg_lang::read_text(path).ok()?;
    let mut out = Vec::new();
    let mut ln = 1u32;
    for line in src.split(|&b| b == b'\n') {
        if ln >= from_line && ln <= to_line {
            out.push(line.strip_suffix(b"\r").unwrap_or(line).to_vec());
        }
        if ln > to_line {
            break;
        }
        ln += 1;
    }
    Some((from_line, out))
}

fn line_of_offset(f: &FileResult, off: u32) -> Option<u32> {
    // approximate via defs (start line known) — used for block/context bounds
    f.defs.iter().find(|d| d.start == off).map(|d| d.line)
}

/// Build the report. `budget == 0` means unlimited content in path/line order (parity mode).
pub fn shape(r: &mut ScanResult) -> Report {
    let o = r.opts.clone();
    let o = &o;
    let mut footer = Footer {
        hits_total: r.stats.total_hits,
        files_total: r.stats.files_matched,
        demoted_files: r.stats.demoted_files,
        demoted_hits: r.stats.demoted_hits,
        minified_hits: r.stats.minified_hits,
        skipped_binary: r.stats.skipped_binary,
        skipped_huge: r.stats.skipped_huge,
        rung: r.rung.clone(),
        ignored_only: r.ignored_only,
        elapsed_ms: r.stats.elapsed_ms,
        ..Default::default()
    };
    // ranked list of (file, hit, score)
    let mut ranked: Vec<(usize, usize, f32)> = Vec::new();
    for (fi, f) in r.files.iter().enumerate() {
        for (hi, h) in f.hits.iter().enumerate() {
            ranked.push((fi, hi, h.score));
        }
    }
    let parity = o.budget == 0;
    if !parity {
        ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal).then(r.files[a.0].rel.cmp(&r.files[b.0].rel)).then(a.1.cmp(&b.1)));
    }

    match o.mode {
        Mode::Files | Mode::Count => {
            let mut files: Vec<ShownFile> = r.files.iter().enumerate().map(|(fi, f)| ShownFile { file: fi, hits: vec![], more: f.total, best: f.prior }).collect();
            if !parity {
                files.sort_by(|a, b| b.best.partial_cmp(&a.best).unwrap().then(r.files[a.file].rel.cmp(&r.files[b.file].rel)));
            }
            let mut est = 0;
            let mut shown = Vec::new();
            for sf in files {
                let cost = tokens::path(r.files[sf.file].rel.len()) + 4;
                if !parity && est + cost > o.budget {
                    break;
                }
                est += cost;
                shown.push(sf);
            }
            footer.files_shown = shown.len();
            footer.hits_shown = shown.iter().map(|s| s.more).sum();
            footer.est_tokens = est;
            hints(&mut footer, r, None);
            return Report { layout: if o.mode == Mode::Files { Layout::Files } else { Layout::Count }, files: shown, facets: None, footer };
        }
        _ => {}
    }

    // Estimated cost of rendering everything in content layout.
    let per_hit = |fi: usize, hi: usize| -> usize {
        let h = &r.files[fi].hits[hi];
        tokens::code(h.text.len()) + 6 + h.chain.iter().map(|(_, n)| n.len() / 4 + 1).sum::<usize>()
    };
    let full_cost: usize = ranked.iter().map(|(fi, hi, _)| per_hit(*fi, *hi)).sum::<usize>() + r.files.iter().map(|f| tokens::path(f.rel.len()) + 3).sum::<usize>();
    let broad = !parity && o.mode == Mode::Content && full_cost > o.budget && r.stats.total_hits > 12;

    let mut layout = match o.mode {
        Mode::Outline => Layout::Outline,
        Mode::Block => Layout::Block,
        _ => Layout::Content,
    };
    let mut facets_out = None;
    let mut est = 0usize;
    let mut selected: Vec<(usize, usize)> = Vec::new();
    if broad {
        layout = Layout::Facets;
        let fc = facets(r, &ranked);
        // facets header cost
        est += 60 + fc.by_dir.len() * 6 + fc.by_flag.len() * 4;
        // definitions first (all of them if they fit, else as many as fit), then top hits
        let mut defs_shown = 0;
        let def_budget = if fc.top_hits.is_empty() { o.budget } else { o.budget * 6 / 10 };
        for &(fi, hi) in &fc.top_defs {
            let c = per_hit(fi, hi) + 6;
            if est + c > def_budget {
                break;
            }
            est += c;
            selected.push((fi, hi));
            defs_shown += 1;
        }
        let mut hits_shown = 0;
        for &(fi, hi) in &fc.top_hits {
            let c = per_hit(fi, hi) + 6;
            if est + c > o.budget || hits_shown >= 10 {
                break;
            }
            est += c;
            selected.push((fi, hi));
            hits_shown += 1;
        }
        let mut fc = fc;
        fc.top_defs.truncate(defs_shown);
        fc.top_hits.truncate(hits_shown);
        facets_out = Some(fc);
    } else {
        // content / outline / block: take ranked hits under the budget with a per-file cap
        let mut per_file: BTreeMap<usize, usize> = BTreeMap::new();
        let cap = if parity { usize::MAX } else { o.per_file_cap.max(1) };
        for &(fi, hi, _) in &ranked {
            let n = per_file.entry(fi).or_default();
            if *n >= cap {
                continue;
            }
            let c = per_hit(fi, hi) + if *n == 0 { tokens::path(r.files[fi].rel.len()) + 3 } else { 0 };
            if !parity && est + c > o.budget {
                if selected.is_empty() {
                    // always show at least one hit
                    selected.push((fi, hi));
                    est += c;
                }
                break;
            }
            est += c;
            *n += 1;
            selected.push((fi, hi));
        }
    }

    // refine the files that will be shown: full outline, precise kinds, chains
    if layout != Layout::Count && layout != Layout::Files {
        let idx: Vec<usize> = selected.iter().map(|(fi, _)| *fi).collect();
        crate::refine(r, &idx);
    }
    // group by file, preserving rank order of first appearance
    let mut files: Vec<ShownFile> = Vec::new();
    let mut index: BTreeMap<usize, usize> = BTreeMap::new();
    for &(fi, hi) in &selected {
        let idx = *index.entry(fi).or_insert_with(|| {
            files.push(ShownFile { file: fi, hits: vec![], more: 0, best: r.files[fi].hits[hi].score });
            files.len() - 1
        });
        files[idx].hits.push(ShownHit { hit: hi, context: None, block: None, seen_before: false });
    }
    for sf in &mut files {
        let f = &r.files[sf.file];
        sf.more = f.total.saturating_sub(sf.hits.len());
        if parity {
            sf.hits.sort_by_key(|h| f.hits[h.hit].line);
        } else {
            sf.hits.sort_by(|a, b| f.hits[b.hit].score.partial_cmp(&f.hits[a.hit].score).unwrap().then(f.hits[a.hit].line.cmp(&f.hits[b.hit].line)));
        }
    }
    if parity {
        files.sort_by(|a, b| r.files[a.file].rel.cmp(&r.files[b.file].rel));
    }

    // adaptive context (content layout only) and blocks
    let total_shown: usize = files.iter().map(|f| f.hits.len()).sum();
    let ctx = match o.context {
        Some(n) => n,
        None if o.before > 0 || o.after > 0 => 0,
        None if parity || layout != Layout::Content => 0,
        None => {
            if r.stats.total_hits <= 1 { 20 } else if r.stats.total_hits <= 3 { 6 } else if r.stats.total_hits <= 10 { 2 } else { 0 }
        }
    };
    let (before, after) = if o.context.is_some() || (o.before == 0 && o.after == 0) { (ctx, ctx) } else { (o.before, o.after) };
    if before + after > 0 && layout == Layout::Content && total_shown <= 64 {
        for sf in &mut files {
            let f = &r.files[sf.file];
            for sh in &mut sf.hits {
                let h = &f.hits[sh.hit];
                let mut from = h.line.saturating_sub(before as u32).max(1);
                let mut to = h.line + after as u32;
                // clip to the enclosing definition when adaptive
                if o.context.is_none() && o.before == 0 && o.after == 0
                    && let Some(di) = h.def_idx {
                        let d = &f.defs[di as usize];
                        from = from.max(d.line);
                        if let Some(end_line) = end_line_of(f, d) {
                            to = to.min(end_line);
                        }
                    }
                if let Some((first, lines)) = read_lines(&f.path, from, to) {
                    est += lines.iter().map(|l| tokens::code(l.len() + 2)).sum::<usize>();
                    sh.context = Some((first, lines));
                }
            }
        }
    }
    if layout == Layout::Block {
        for sf in &mut files {
            let f = &r.files[sf.file];
            let mut seen: Vec<u32> = Vec::new();
            for sh in &mut sf.hits {
                let h = &f.hits[sh.hit];
                let Some(di) = h.def_idx else { continue };
                if seen.contains(&di) {
                    continue;
                }
                seen.push(di);
                let d = &f.defs[di as usize];
                let to = end_line_of(f, d).unwrap_or(d.line + 40).min(d.line + 200);
                if let Some((first, lines)) = read_lines(&f.path, d.line, to) {
                    est += lines.iter().map(|l| tokens::code(l.len() + 2)).sum::<usize>();
                    sh.block = Some((first, lines));
                }
            }
        }
    }

    footer.hits_shown = total_shown;
    footer.files_shown = files.len();
    footer.est_tokens = est + 40;
    hints(&mut footer, r, facets_out.as_ref());
    Report { layout, files, facets: facets_out, footer }
}

fn end_line_of(f: &FileResult, d: &crate::DefSummary) -> Option<u32> {
    // count newlines between start and end by reading the file segment
    let src = greeg_lang::read_text(&f.path).ok()?;
    let seg = &src[d.start as usize..(d.end as usize).min(src.len())];
    Some(d.line + memchr::memchr_iter(b'\n', seg).count() as u32 - if seg.ends_with(b"\n") { 1 } else { 0 })
}

fn hints(footer: &mut Footer, r: &ScanResult, facets: Option<&Facets>) {
    let o = &r.opts;
    let pat = &o.pattern;
    if r.stats.total_hits == 0 {
        if let Some((files, hits)) = r.ignored_only {
            footer.hints.push(format!("{hits} hits in {files} ignored/hidden files: add --no-ignore --hidden"));
        } else {
            footer.hints.push("no hits after the escalation ladder; try a shorter or split identifier".into());
        }
        return;
    }
    if let Some(fc) = facets {
        if fc.word_hits > 0 && fc.word_hits < r.stats.total_hits.min(64) && !o.word {
            footer.hints.push(format!("-w {pat}"));
        }
        if fc.defs_total > 0 && o.kinds.is_empty() {
            footer.hints.push(format!("{pat} --kind def"));
        }
        if let Some((d, _)) = fc.by_dir.first() {
            footer.hints.push(format!("{pat} -g '{d}/**'"));
        }
        if r.stats.demoted_hits > 0 && !o.no_tests {
            footer.hints.push(format!("{pat} --no-tests"));
        }
    } else if footer.hits_shown < footer.hits_total {
        footer.hints.push(format!("{pat} --budget {}", o.budget * 2));
        if r.stats.by_kind[HitKind::Def.idx()] > 0 && o.kinds.is_empty() {
            footer.hints.push(format!("{pat} --kind def"));
        }
    }
    if r.stats.total_hits > 0 && r.stats.by_kind[HitKind::Def.idx()] == 0 && o.mode == Mode::Content && !o.no_ignore {
        footer.hints.push("no definitions found in searched files; the symbol may be defined in a dependency or generated code".into());
    }
    let _ = DefKind::Function;
    let _ = FileFlags::TEST;
    let _ = line_of_offset;
}
