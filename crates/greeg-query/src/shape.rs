//! Shape a `ScanResult` to a budget: choose content vs facets, rank, cap per
//! file, add adaptive context, and compute the footer (DESIGN.md §6.4, §10,
//! OUTPUT.md). Every emitted line is accounted for against the budget:
//! file headers, `+N more` lines, collapsed groups, footer and hints.

use crate::{FileResult, HitKind, Mode, Rung, ScanResult, is_mock_path, tokens};
use greeg_lang::FileFlags;
use std::cmp::Ordering;
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
    /// Signature line of the enclosing definition when it lies outside `context`
    /// (single call/ident hit, OUTPUT.md "adaptive context").
    pub sig: Option<(u32, Vec<u8>)>,
    /// Whole enclosing block (block layout).
    pub block: Option<(u32, Vec<Vec<u8>>)>,
    /// The block was cut to fit the budget.
    pub block_clipped: bool,
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
    /// Areas: shortest distinguishing directory prefixes, source first, top 6.
    pub by_dir: Vec<(String, usize)>,
    /// Empty unless at least two languages hold ≥ 5 % of the hits each.
    pub by_lang: Vec<(String, usize)>,
    /// Demoted flags with hit counts (`test`, `vendored`, `generated`, `mock`).
    pub by_flag: Vec<(&'static str, usize)>,
    /// (file index, hit index) of the definition hits to show, best first.
    pub top_defs: Vec<(usize, usize)>,
    /// Collapsed demoted definitions: (group, count).
    pub demoted_defs: Vec<(&'static str, usize)>,
    /// Non-definition, non-import hits to show, best first.
    pub top_hits: Vec<(usize, usize)>,
    /// Files with import hits, best first (rendered as `imported by N files: …`).
    pub import_files: Vec<usize>,
    pub defs_total: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Footer {
    pub hits_shown: usize,
    /// Matched lines (after `--kind`).
    pub hits_total: usize,
    /// Matched lines before `--kind` filtering (equal to `hits_total` without a filter).
    pub hits_unfiltered: usize,
    pub files_shown: usize,
    pub files_total: usize,
    pub demoted_files: usize,
    pub demoted_hits: usize,
    pub minified_hits: usize,
    pub skipped_binary: usize,
    pub skipped_huge: usize,
    pub rung: Rung,
    pub ignored_only: Option<(usize, usize)>,
    /// `ignored_only` is a lower bound (rung 5 stopped at its bound).
    pub ignored_partial: bool,
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

/// Token reserve for the footer line and its hints.
const FOOTER_COST: usize = 45;
/// Reserve per file group for a possible `+N more` line.
const MORE_COST: usize = 7;

fn kind_counts(r: &ScanResult) -> Vec<(HitKind, usize)> {
    HitKind::ALL
        .iter()
        .map(|k| (*k, r.stats.by_kind[k.idx()]))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// A file is demoted for layout purposes: flagged, or on a mock/stub path.
pub fn demoted(f: &FileResult, all: bool) -> bool {
    !all && (f.flags.demoted() || is_mock_path(&f.rel))
}

/// Group name of a demoted file (`test`, `vendored`, `generated`, `mock`).
pub fn demote_group(f: &FileResult) -> &'static str {
    if f.flags.has(FileFlags::TEST) {
        "test"
    } else if f.flags.has(FileFlags::VENDORED) {
        "vendored"
    } else if f
        .flags
        .has(FileFlags::GENERATED | FileFlags::MINIFIED | FileFlags::LOCKFILE)
    {
        "generated"
    } else if is_mock_path(&f.rel) {
        "mock"
    } else {
        "demoted"
    }
}

fn is_demoted_dir(dir: &str) -> bool {
    let probe = if dir.is_empty() {
        "x.rs".to_string()
    } else {
        format!("{dir}/x.rs")
    };
    greeg_lang::path_flags(&probe).demoted() || is_mock_path(&probe)
}

/// Areas: directory prefixes that distinguish where the hits are. Starts at
/// depth 1 and splits every group holding more than a fifth of the hits, so
/// `tokio/src/sync 300  tokio/src/runtime 200` rather than `tokio 900`.
fn areas(r: &ScanResult) -> Vec<(String, usize)> {
    let dirs: Vec<(Vec<&str>, usize)> = r
        .files
        .iter()
        .map(|f| {
            (
                f.rel
                    .rsplit_once('/')
                    .map(|(d, _)| d.split('/').collect::<Vec<_>>())
                    .unwrap_or_default(),
                f.total,
            )
        })
        .collect();
    let total: usize = dirs.iter().map(|d| d.1).sum();
    let group_at = |prefix: &[&str], depth: usize| -> Vec<(Vec<&str>, usize)> {
        let mut m: BTreeMap<Vec<&str>, usize> = BTreeMap::new();
        for (d, n) in &dirs {
            if d.len() >= prefix.len() && d[..prefix.len()] == *prefix {
                let key: Vec<&str> = d[..depth.min(d.len())].to_vec();
                *m.entry(key).or_default() += n;
            }
        }
        m.into_iter().collect()
    };
    // split every group holding more than a fifth of the hits into its
    // subdirectories (files directly in the group's directory stay as a leaf)
    let mut groups = group_at(&[], 1);
    let mut done: Vec<Vec<&str>> = Vec::new();
    for _ in 0..12 {
        let Some((i, (prefix, count))) = groups
            .iter()
            .enumerate()
            .filter(|(_, g)| !done.contains(&g.0))
            .max_by_key(|(_, g)| g.1)
            .map(|(i, g)| (i, g.clone()))
        else {
            break;
        };
        if count * 5 <= total {
            break;
        }
        let children = group_at(&prefix, prefix.len() + 1);
        // the leaf child (files directly in the directory) keeps the prefix as its key
        done.push(prefix);
        if children.len() < 2 {
            continue;
        }
        groups.remove(i);
        groups.extend(children);
    }
    let mut out: Vec<(String, usize, bool)> = groups
        .into_iter()
        .map(|(p, n)| {
            let name = if p.is_empty() {
                "(root)".to_string()
            } else {
                p.join("/")
            };
            let dem = is_demoted_dir(&name);
            (name, n, dem)
        })
        .collect();
    out.sort_by(|a, b| a.2.cmp(&b.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
    out.truncate(6);
    out.into_iter().map(|(n, c, _)| (n, c)).collect()
}

type Ranked = (usize, usize, f32);
/// (file index, hit index) of a selected hit.
type Sel = (usize, usize);

fn rank_cmp(r: &ScanResult) -> impl Fn(&Ranked, &Ranked) -> Ordering + '_ {
    move |a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(Ordering::Equal)
            .then_with(|| r.files[a.0].rel.cmp(&r.files[b.0].rel))
            .then(a.1.cmp(&b.1))
    }
}

/// Order the best `k` entries first (the rest unordered) — a partial sort.
fn top_k(v: &mut [Ranked], k: usize, r: &ScanResult) {
    let cmp = rank_cmp(r);
    if k < v.len() {
        v.select_nth_unstable_by(k, &cmp);
        v[..k].sort_by(&cmp);
    } else {
        v.sort_by(&cmp);
    }
}

/// Facets and the ranked definition / top-hit lists; `ranked` need not be sorted.
/// Returns the facets and the number of whole-word exact matches (for the `-w` hint).
fn facets(r: &ScanResult, ranked: &[Ranked]) -> (Facets, usize) {
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_flag: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut word_hits = 0usize;
    let pat_len = r.opts.pattern.len() as u32;
    let all = r.opts.all;
    for f in &r.files {
        *by_lang.entry(f.lang.short().to_string()).or_default() += f.total;
        if demoted(f, all) {
            *by_flag.entry(demote_group(f)).or_default() += f.total;
        }
        for h in &f.hits {
            if h.match_end - h.match_start == pat_len {
                word_hits += 1;
            }
        }
    }
    let total = r.stats.total_hits.max(1);
    let mut by_lang: Vec<_> = by_lang
        .into_iter()
        .filter(|(_, n)| n * 20 >= total)
        .collect();
    by_lang.sort_by_key(|x| std::cmp::Reverse(x.1));
    if by_lang.len() < 2 {
        by_lang.clear();
    }
    let mut by_flag: Vec<_> = by_flag.into_iter().collect();
    by_flag.sort_by_key(|x| std::cmp::Reverse(x.1));
    let mut defs: Vec<Ranked> = ranked
        .iter()
        .filter(|(fi, hi, _)| r.files[*fi].hits[*hi].kind == HitKind::Def)
        .copied()
        .collect();
    defs.sort_by(rank_cmp(r));
    let mut others: Vec<Ranked> = ranked
        .iter()
        .filter(|(fi, hi, _)| {
            !matches!(r.files[*fi].hits[*hi].kind, HitKind::Def | HitKind::Import)
        })
        .copied()
        .collect();
    top_k(&mut others, 12, r);
    let top_defs: Vec<(usize, usize)> = defs.iter().map(|(fi, hi, _)| (*fi, *hi)).collect();
    let defs_total = top_defs.len();
    let top_hits: Vec<(usize, usize)> = others
        .iter()
        .take(12)
        .map(|(fi, hi, _)| (*fi, *hi))
        .collect();
    let mut import_files: Vec<usize> = r
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kinds[HitKind::Import.idx()] > 0)
        .map(|(i, _)| i)
        .collect();
    import_files.sort_by(|a, b| {
        r.files[*b]
            .prior
            .partial_cmp(&r.files[*a].prior)
            .unwrap_or(Ordering::Equal)
            .then(r.files[*a].rel.cmp(&r.files[*b].rel))
    });
    (
        Facets {
            by_kind: kind_counts(r),
            by_dir: areas(r),
            by_lang,
            by_flag,
            top_defs,
            demoted_defs: vec![],
            top_hits,
            import_files,
            defs_total,
        },
        word_hits,
    )
}

/// Lines `from..=to` of a file, from its once-read source.
fn read_lines(f: &mut FileResult, from_line: u32, to_line: u32) -> Option<(u32, Vec<Vec<u8>>)> {
    let src = f.source()?;
    // a trailing newline does not start a line (ripgrep prints nothing after it)
    let last = if src.bytes.ends_with(b"\n") {
        src.line_count().saturating_sub(1)
    } else {
        src.line_count()
    };
    Some(src.lines(from_line, to_line.min(last.max(from_line))))
}

/// Display cost of one hit line: text, line number, kind and container.
fn per_hit(r: &ScanResult, fi: usize, hi: usize) -> usize {
    let h = &r.files[fi].hits[hi];
    let container: usize = h
        .chain
        .iter()
        .rev()
        .take(2)
        .map(|(_, n)| n.len() / 4 + 1)
        .sum();
    tokens::code(&h.text) + 4 + container
}

fn header_cost(r: &ScanResult, fi: usize) -> usize {
    tokens::path(r.files[fi].rel.as_bytes()) + 2 + MORE_COST
}

/// Build the report. `budget == 0` means unlimited content in path/line order (parity mode).
pub fn shape(r: &mut ScanResult) -> Report {
    let o = r.opts.clone();
    let o = &o;
    let mut footer = Footer {
        hits_total: r.stats.total_hits,
        hits_unfiltered: r.stats.total_unfiltered,
        files_total: r.stats.files_matched,
        demoted_files: r.stats.demoted_files,
        demoted_hits: r.stats.demoted_hits,
        minified_hits: r.stats.minified_hits,
        skipped_binary: r.stats.skipped_binary,
        skipped_huge: r.stats.skipped_huge,
        rung: r.rung.clone(),
        ignored_only: r.ignored_only,
        ignored_partial: r.ignored_partial,
        elapsed_ms: r.stats.elapsed_ms,
        ..Default::default()
    };
    let parity = o.budget == 0;

    match o.mode {
        Mode::Files | Mode::Count => {
            // C2: never truncated by the budget; source files first unless --sort path or --budget 0
            let mut files: Vec<ShownFile> = r
                .files
                .iter()
                .enumerate()
                .map(|(fi, f)| ShownFile {
                    file: fi,
                    hits: vec![],
                    more: f.total,
                    best: f.prior,
                })
                .collect();
            if !parity && !o.sort_path {
                files.sort_by(|a, b| {
                    b.best
                        .partial_cmp(&a.best)
                        .unwrap()
                        .then(r.files[a.file].rel.cmp(&r.files[b.file].rel))
                });
            } else {
                files.sort_by(|a, b| r.files[a.file].rel.cmp(&r.files[b.file].rel));
            }
            let est: usize = files
                .iter()
                .map(|sf| tokens::path(r.files[sf.file].rel.as_bytes()) + 2)
                .sum();
            footer.files_shown = files.len();
            footer.hits_shown = files.iter().map(|s| s.more).sum();
            footer.est_tokens = est + 20;
            hints(&mut footer, r, None, 0);
            return Report {
                layout: if o.mode == Mode::Files {
                    Layout::Files
                } else {
                    Layout::Count
                },
                files,
                facets: None,
                footer,
            };
        }
        _ => {}
    }

    // (file, hit, score) for every retained hit; sorted only as far as the budget needs
    let mut ranked: Vec<Ranked> = Vec::with_capacity(r.files.iter().map(|f| f.hits.len()).sum());
    for (fi, f) in r.files.iter().enumerate() {
        for (hi, h) in f.hits.iter().enumerate() {
            ranked.push((fi, hi, h.score));
        }
    }
    // Estimated cost of rendering everything in content layout.
    let full_cost: usize = ranked
        .iter()
        .map(|(fi, hi, _)| per_hit(r, *fi, *hi))
        .sum::<usize>()
        + r.files
            .iter()
            .enumerate()
            .map(|(fi, _)| header_cost(r, fi))
            .sum::<usize>()
        + FOOTER_COST;
    let broad =
        !parity && o.mode == Mode::Content && full_cost > o.budget && r.stats.total_hits > 12;

    let mut layout = match o.mode {
        Mode::Outline => Layout::Outline,
        Mode::Block => Layout::Block,
        _ => Layout::Content,
    };
    let mut facets_out = None;
    let mut word_hits = 0usize;
    let mut est = FOOTER_COST;
    let mut selected: Vec<(usize, usize)> = Vec::new();
    if broad {
        layout = Layout::Facets;
        let (mut fc, wh) = facets(r, &ranked);
        word_hits = wh;
        // facets header: two lines
        est += 14
            + fc.by_kind.len() * 4
            + fc.by_flag.len() * 4
            + fc.by_dir.len() * 8
            + fc.by_lang.len() * 4;
        if !fc.import_files.is_empty() {
            est += 8 + fc.import_files.len().min(6) * 5;
        }
        // definitions first: source definitions, then demoted ones capped at a
        // quarter of the block, the rest collapsed to one line per group
        let mut headers: Vec<usize> = Vec::new();
        let mut charge = |est: &mut usize, fi: usize, hi: usize| -> usize {
            let mut c = per_hit(r, fi, hi);
            if !headers.contains(&fi) {
                c += header_cost(r, fi) - MORE_COST;
            }
            if *est + c <= o.budget {
                *est += c;
                if !headers.contains(&fi) {
                    headers.push(fi);
                }
                c
            } else {
                0
            }
        };
        let def_budget = if fc.top_hits.is_empty() {
            o.budget
        } else {
            o.budget * 6 / 10
        };
        let (src_defs, dem_defs): (Vec<Sel>, Vec<Sel>) = fc
            .top_defs
            .iter()
            .copied()
            .partition(|(fi, _)| !demoted(&r.files[*fi], o.all));
        let mut shown_defs: Vec<(usize, usize)> = Vec::new();
        est += 6; // "definitions (N of M)"
        for &(fi, hi) in &src_defs {
            if est + per_hit(r, fi, hi) > def_budget {
                break;
            }
            if charge(&mut est, fi, hi) > 0 {
                shown_defs.push((fi, hi));
            }
        }
        let allow_dem = if shown_defs.is_empty() {
            3
        } else {
            shown_defs.len() / 3
        };
        let mut collapsed: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut dem_shown = 0usize;
        for &(fi, hi) in &dem_defs {
            if dem_shown < allow_dem
                && est + per_hit(r, fi, hi) <= def_budget
                && charge(&mut est, fi, hi) > 0
            {
                shown_defs.push((fi, hi));
                dem_shown += 1;
            } else {
                *collapsed.entry(demote_group(&r.files[fi])).or_default() += 1;
            }
        }
        est += collapsed.len() * 8;
        let mut hits_shown = 0;
        est += 3; // "top hits"
        let mut shown_hits: Vec<(usize, usize)> = Vec::new();
        for &(fi, hi) in &fc.top_hits {
            if hits_shown >= 10 || est + per_hit(r, fi, hi) > o.budget {
                break;
            }
            if charge(&mut est, fi, hi) > 0 {
                shown_hits.push((fi, hi));
                hits_shown += 1;
            }
        }
        selected.extend(shown_defs.iter().copied());
        selected.extend(shown_hits.iter().copied());
        fc.top_defs = shown_defs;
        fc.top_hits = shown_hits;
        fc.demoted_defs = collapsed.into_iter().collect();
        facets_out = Some(fc);
    } else {
        // content / outline / block: take ranked hits under the budget with a per-file cap.
        // The cap is budget-driven when few files matched or a single file was named.
        let single_file =
            o.paths.len() == 1 && (o.paths[0].is_file() || o.root.join(&o.paths[0]).is_file());
        let cap = if parity || r.files.len() <= 3 || single_file {
            usize::MAX
        } else {
            o.per_file_cap.max(1)
        };
        let mut per_file: BTreeMap<usize, usize> = BTreeMap::new();
        // each shown hit costs at least 7 tokens: only that many entries need ordering
        let mut k = if parity {
            ranked.len()
        } else {
            (o.budget / 7 + 8).min(ranked.len())
        };
        if !parity {
            top_k(&mut ranked, k, r);
        }
        let mut pos = 0usize;
        loop {
            if pos == k {
                if k == ranked.len() {
                    break;
                }
                // the cap skipped entries and the budget is not spent: order the rest too
                ranked[k..].sort_by(rank_cmp(r));
                k = ranked.len();
            }
            let (fi, hi, _) = ranked[pos];
            pos += 1;
            let n = per_file.entry(fi).or_default();
            if *n >= cap {
                continue;
            }
            let c = per_hit(r, fi, hi) + if *n == 0 { header_cost(r, fi) } else { 0 };
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
            files.push(ShownFile {
                file: fi,
                hits: vec![],
                more: 0,
                best: r.files[fi].hits[hi].score,
            });
            files.len() - 1
        });
        files[idx].hits.push(ShownHit {
            hit: hi,
            context: None,
            sig: None,
            block: None,
            block_clipped: false,
            seen_before: false,
        });
    }
    for sf in &mut files {
        let f = &r.files[sf.file];
        sf.more = f.total.saturating_sub(sf.hits.len());
        if parity {
            sf.hits.sort_by_key(|h| f.hits[h.hit].line);
        } else {
            sf.hits.sort_by(|a, b| {
                f.hits[b.hit]
                    .score
                    .partial_cmp(&f.hits[a.hit].score)
                    .unwrap()
                    .then(f.hits[a.hit].line.cmp(&f.hits[b.hit].line))
            });
        }
    }
    if parity {
        files.sort_by(|a, b| r.files[a.file].rel.cmp(&r.files[b.file].rel));
    }

    // adaptive context (content layout only) and blocks
    let total_shown: usize = files.iter().map(|f| f.hits.len()).sum();
    let single_def = r.stats.total_hits == 1
        && files
            .first()
            .and_then(|sf| sf.hits.first())
            .map(|sh| r.files[files[0].file].hits[sh.hit].kind == HitKind::Def)
            .unwrap_or(false);
    let adaptive = !o.explicit_context() && !parity && layout == Layout::Content;
    let (before, after) = if let Some(n) = o.context {
        (n, n)
    } else if o.explicit_context() {
        (o.before, o.after)
    } else if !adaptive {
        (0, 0)
    } else if r.stats.total_hits == 1 {
        if single_def { (20, 20) } else { (2, 4) }
    } else if r.stats.total_hits <= 3 {
        (2, 2)
    } else if r.stats.total_hits <= 10 {
        (1, 1)
    } else {
        (0, 0)
    };
    if before + after > 0
        && layout == Layout::Content
        && (total_shown <= 64 || o.explicit_context())
    {
        for sf in &mut files {
            let f = &mut r.files[sf.file];
            for sh in &mut sf.hits {
                let h = &f.hits[sh.hit];
                let mut from = h.line.saturating_sub(before as u32).max(1);
                let mut to = h.line + after as u32;
                // clip to the enclosing definition when adaptive
                if adaptive && let Some(di) = h.def_idx {
                    let dl = f.defs[di as usize].line;
                    from = from.max(dl);
                    if let Some(end_line) = end_line_of(f, di as usize) {
                        to = to.min(end_line);
                    }
                    if r.stats.total_hits == 1
                        && !single_def
                        && dl < from
                        && let Some((_, mut l)) = read_lines(f, dl, dl)
                        && let Some(line) = l.pop()
                    {
                        est += tokens::code(&line) + 3;
                        sh.sig = Some((dl, line));
                    }
                }
                if let Some((first, lines)) = read_lines(f, from, to) {
                    let cost = lines.iter().map(|l| tokens::code(l) + 2).sum::<usize>();
                    if !parity && !o.explicit_context() && est + cost > o.budget + o.budget / 4 {
                        continue;
                    }
                    est += cost;
                    sh.context = Some((first, lines));
                }
            }
        }
    }
    if layout == Layout::Block {
        for sf in &mut files {
            let f = &mut r.files[sf.file];
            let mut seen: Vec<u32> = Vec::new();
            for sh in &mut sf.hits {
                let h = &f.hits[sh.hit];
                let Some(di) = h.def_idx else { continue };
                if seen.contains(&di) {
                    continue;
                }
                seen.push(di);
                let d_line = f.defs[di as usize].line;
                let to = end_line_of(f, di as usize)
                    .unwrap_or(d_line + 40)
                    .min(d_line + 200);
                if let Some((first, mut lines)) = read_lines(f, d_line, to) {
                    if !parity {
                        // bodies count against the budget: cap, keep at least three lines
                        let mut used = 0usize;
                        let mut keep = 0usize;
                        for l in &lines {
                            let c = tokens::code(l) + 2;
                            if keep >= 3 && est + used + c + 4 > o.budget {
                                break;
                            }
                            used += c;
                            keep += 1;
                        }
                        if keep < lines.len() {
                            lines.truncate(keep);
                            sh.block_clipped = true;
                            used += 4;
                        }
                        est += used;
                    }
                    sh.block = Some((first, lines));
                }
            }
        }
    }

    footer.hits_shown = total_shown;
    footer.files_shown = files.len();
    footer.est_tokens = est;
    hints(&mut footer, r, facets_out.as_ref(), word_hits);
    Report {
        layout,
        files,
        facets: facets_out,
        footer,
    }
}

/// Last line of definition `di` of `f` (from the once-read source).
fn end_line_of(f: &mut FileResult, di: usize) -> Option<u32> {
    let (start, end) = (f.defs[di].start, f.defs[di].end);
    let src = f.source()?;
    Some(src.end_line(start, end))
}

fn is_identifier(p: &str) -> bool {
    !p.is_empty()
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !p.bytes().next().unwrap().is_ascii_digit()
}

/// Footer hints: flags only, never a demoted area, never a pattern repetition.
fn hints(footer: &mut Footer, r: &ScanResult, facets: Option<&Facets>, word_hits: usize) {
    let o = &r.opts;
    if r.stats.total_hits == 0 {
        if let Some((files, hits)) = r.ignored_only {
            let more = if r.ignored_partial { "+" } else { "" };
            footer.hints.push(format!(
                "{hits}{more} hits in {files}{more} ignored/hidden files: add --no-ignore --hidden"
            ));
        } else {
            footer.hints.push(
                "no hits after the escalation ladder; try a shorter or split identifier".into(),
            );
        }
        return;
    }
    if let Some(fc) = facets {
        if word_hits > 0 && word_hits < r.stats.total_hits.min(64) && !o.word {
            footer.hints.push("-w".into());
        }
        if fc.defs_total > 0 && o.kinds.is_empty() {
            footer.hints.push("--kind def".into());
        }
        if let Some((d, _)) = fc
            .by_dir
            .iter()
            .find(|(d, _)| d != "(root)" && !is_demoted_dir(d))
        {
            footer.hints.push(format!("-g '{d}/**'"));
        }
        if r.stats.demoted_hits > 0 && !o.no_tests {
            footer.hints.push("--no-tests".into());
        }
    } else if footer.hits_shown < footer.hits_total {
        footer.hints.push(format!("--budget {}", o.budget * 2));
        if r.stats.by_kind[HitKind::Def.idx()] > 0 && o.kinds.is_empty() {
            footer.hints.push("--kind def".into());
        }
    }
    if r.stats.total_hits > 0
        && r.stats.by_kind[HitKind::Def.idx()] == 0
        && o.mode == Mode::Content
        && !o.no_ignore
        && is_identifier(&o.pattern)
    {
        footer
            .hints
            .push("no definition in searched files (a dependency or generated code?)".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hit, Options, Stats};
    use greeg_lang::Lang;

    fn hit(line: u32, score: f32, kind: HitKind) -> Hit {
        Hit {
            line,
            line_start: 0,
            match_start: 0,
            match_end: 3,
            submatches: vec![(0, 3)],
            kind,
            chain: vec![],
            def_idx: None,
            score,
            text: b"foo bar".to_vec(),
            text_match: (0, 3),
            clipped: false,
            raw: b"foo bar".to_vec(),
        }
    }

    fn file(rel: &str, hits: Vec<Hit>) -> FileResult {
        let total = hits.len();
        let mut kinds = [0u32; 9];
        for h in &hits {
            kinds[h.kind.idx()] += 1;
        }
        FileResult {
            rel: rel.into(),
            path: rel.into(),
            lang: Lang::Rust,
            flags: greeg_lang::path_flags(rel),
            size: 0,
            age_days: 0.0,
            mtime: 0,
            prior: 0.8,
            hits,
            total,
            total_unfiltered: total,
            kinds,
            defs: vec![],
            refined: true,
            file_id: None,
            src: None,
        }
    }

    fn result(files: Vec<FileResult>, o: Options) -> ScanResult {
        let mut stats = Stats::default();
        for f in &files {
            stats.files_matched += 1;
            stats.total_hits += f.total;
            stats.total_unfiltered += f.total_unfiltered;
            for k in 0..9 {
                stats.by_kind[k] += f.kinds[k] as usize;
            }
            if f.flags.demoted() {
                stats.demoted_files += 1;
                stats.demoted_hits += f.total;
            }
        }
        ScanResult {
            opts: o,
            files,
            stats,
            rung: Rung::Exact,
            ignored_only: None,
            ignored_partial: false,
        }
    }

    #[test]
    fn per_file_cap_is_budget_driven_for_few_files() {
        let hits: Vec<Hit> = (1..=20).map(|i| hit(i, 0.5, HitKind::Ident)).collect();
        let o = Options {
            pattern: "foo".into(),
            budget: 2000,
            ..Default::default()
        };
        let mut r = result(vec![file("a.rs", hits.clone())], o.clone());
        let rep = shape(&mut r);
        assert_eq!(
            rep.files[0].hits.len(),
            20,
            "a single matched file shows every hit the budget allows"
        );
        // five files: the cap applies again
        let mut r = result(
            (0..5)
                .map(|i| file(&format!("f{i}.rs"), hits.clone()))
                .collect(),
            o,
        );
        let rep = shape(&mut r);
        assert!(rep.files.iter().all(|f| f.hits.len() <= 4));
        assert_eq!(rep.footer.hits_total, 100);
    }

    #[test]
    fn top_k_selection_matches_full_sort() {
        let mut files = Vec::new();
        for i in 0..30 {
            let hits: Vec<Hit> = (1..=10)
                .map(|l| {
                    hit(
                        l,
                        ((i * 7 + l as usize * 3) % 17) as f32 / 17.0,
                        HitKind::Call,
                    )
                })
                .collect();
            files.push(file(&format!("d/f{i:02}.rs"), hits));
        }
        let o = Options {
            pattern: "foo".into(),
            budget: 300,
            ..Default::default()
        };
        let mut r = result(files, o);
        let rep = shape(&mut r);
        // every shown hit scores at least as high as every hit not shown (cap aside)
        let rf = &r.files;
        let shown_min = rep
            .files
            .iter()
            .flat_map(|sf| sf.hits.iter().map(move |sh| rf[sf.file].hits[sh.hit].score))
            .fold(f32::MAX, f32::min);
        let mut unshown_max = 0f32;
        for (fi, f) in r.files.iter().enumerate() {
            let shown: Vec<usize> = rep
                .files
                .iter()
                .filter(|sf| sf.file == fi)
                .flat_map(|sf| sf.hits.iter().map(|h| h.hit))
                .collect();
            if shown.len() >= 4 {
                continue; // capped file: its remaining hits may outrank shown ones elsewhere
            }
            for (hi, h) in f.hits.iter().enumerate() {
                if !shown.contains(&hi) {
                    unshown_max = unshown_max.max(h.score);
                }
            }
        }
        assert!(
            shown_min >= unshown_max,
            "shown {shown_min} unshown {unshown_max}"
        );
        assert!(rep.footer.hits_shown > 0 && rep.footer.hits_shown < 300);
    }

    #[test]
    fn kind_filter_footer_counts_filtered_set() {
        let mut f = file("a.rs", vec![hit(3, 1.0, HitKind::Def)]);
        f.total_unfiltered = 31;
        let o = Options {
            pattern: "foo".into(),
            kinds: vec![HitKind::Def],
            ..Default::default()
        };
        let mut r = result(vec![f], o);
        let rep = shape(&mut r);
        assert_eq!(rep.footer.hits_total, 1);
        assert_eq!(rep.footer.hits_unfiltered, 31);
        assert_eq!(rep.files[0].more, 0);
    }

    #[test]
    fn files_mode_is_never_truncated_and_sorts_source_first() {
        let mut files: Vec<FileResult> = (0..40)
            .map(|i| {
                file(
                    &format!("tests/t{i:02}.rs"),
                    vec![hit(1, 0.4, HitKind::Ident)],
                )
            })
            .collect();
        for f in &mut files {
            f.prior = 0.36;
        }
        files.push(file("src/lib.rs", vec![hit(1, 0.8, HitKind::Ident)]));
        let o = Options {
            pattern: "foo".into(),
            mode: Mode::Files,
            budget: 50,
            ..Default::default()
        };
        let mut r = result(files, o.clone());
        let rep = shape(&mut r);
        assert_eq!(rep.layout, Layout::Files);
        assert_eq!(
            rep.files.len(),
            41,
            "-l lists every file regardless of the budget"
        );
        assert_eq!(r.files[rep.files[0].file].rel, "src/lib.rs");
        let mut r = result(
            r.files.clone(),
            Options {
                sort_path: true,
                ..o
            },
        );
        let rep = shape(&mut r);
        assert_eq!(r.files[rep.files[0].file].rel, "src/lib.rs");
        assert_eq!(r.files[rep.files[1].file].rel, "tests/t00.rs");
    }

    #[test]
    fn facets_collapse_demoted_definitions_and_imports() {
        let mut files = Vec::new();
        for i in 0..6 {
            let mut v: Vec<Hit> = (1..=8).map(|l| hit(l, 0.4, HitKind::Call)).collect();
            v.push(hit(9, 0.8, HitKind::Def));
            v.push(hit(10, 0.3, HitKind::Import));
            files.push(file(&format!("src/m{i}.rs"), v));
        }
        for i in 0..20 {
            let mut v: Vec<Hit> = (1..=4).map(|l| hit(l, 0.2, HitKind::Call)).collect();
            v.push(hit(5, 0.45, HitKind::Def));
            files.push(file(&format!("tests/t{i:02}.rs"), v));
        }
        let o = Options {
            pattern: "foo".into(),
            budget: 600,
            ..Default::default()
        };
        let mut r = result(files, o);
        let rep = shape(&mut r);
        assert_eq!(rep.layout, Layout::Facets);
        let fc = rep.facets.as_ref().unwrap();
        assert_eq!(fc.defs_total, 26);
        let dem_shown = fc
            .top_defs
            .iter()
            .filter(|(fi, _)| demoted(&r.files[*fi], false))
            .count();
        assert!(
            dem_shown <= fc.top_defs.len() / 3,
            "demoted definitions take at most a quarter of the block"
        );
        assert_eq!(
            fc.demoted_defs.iter().map(|(_, n)| n).sum::<usize>(),
            20 - dem_shown
        );
        assert_eq!(fc.demoted_defs[0].0, "test");
        assert!(
            fc.top_hits
                .iter()
                .all(|(fi, hi)| r.files[*fi].hits[*hi].kind != HitKind::Import)
        );
        assert_eq!(fc.import_files.len(), 6);
        assert_eq!(fc.by_dir[0].0, "src", "source areas first");
        assert!(rep.footer.hints.iter().any(|h| h == "--kind def"));
        assert!(rep.footer.hints.iter().any(|h| h == "-g 'src/**'"));
        assert!(
            rep.footer.est_tokens <= 600 + 60,
            "accounting stays near the budget: {}",
            rep.footer.est_tokens
        );
    }

    #[test]
    fn areas_split_the_dominant_group() {
        let mut files = Vec::new();
        for (d, n) in [
            ("tokio/src/sync", 30),
            ("tokio/src/runtime", 20),
            ("tokio/tests", 10),
            ("benches", 2),
        ] {
            for i in 0..n {
                files.push(file(
                    &format!("{d}/f{i}.rs"),
                    vec![hit(1, 0.5, HitKind::Call)],
                ));
            }
        }
        let r = result(files, Options::default());
        let a = areas(&r);
        assert_eq!(a[0].0, "tokio/src/sync");
        assert_eq!(a[1].0, "tokio/src/runtime");
        assert!(
            a.iter().position(|(d, _)| d == "tokio/tests").unwrap() > 1,
            "test areas never first"
        );
    }
}
