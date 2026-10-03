//! `--json=greeg`: greeg's own JSON Lines, schema 2 (references/JSON.md).
//! Every record is `{"type":…,"data":{…}}`; the first one names the schema.
//! Paths and file content are `{"text"}` or `{"bytes"}`, never replaced.

use crate::{FileLine, chain_str, file_lines};
use anyhow::Result;
use greeg_query::outcome::Outcome;
use greeg_query::shape::{Layout, Report, ShownFile};
use greeg_query::verbs::{self, DefEntry};
use greeg_query::{FileResult, HitKind, ScanResult};
use serde::Serialize;
use std::io::Write;

pub(crate) const SCHEMA: u32 = 2;

/// Bytes from a file or a path: the string when UTF-8, otherwise base64.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Text<'a> {
    Text(&'a str),
    Bytes(String),
}

impl<'a> Text<'a> {
    pub(crate) fn of(b: &'a [u8]) -> Text<'a> {
        match std::str::from_utf8(b) {
            Ok(s) => Text::Text(s),
            Err(_) => Text::Bytes(crate::base64(b)),
        }
    }
}

#[derive(Serialize)]
struct Record<'a, T> {
    r#type: &'a str,
    data: &'a T,
}

fn put<T: Serialize>(w: &mut dyn Write, kind: &str, data: &T) -> Result<()> {
    serde_json::to_writer(&mut *w, &Record { r#type: kind, data })?;
    writeln!(w)?;
    Ok(())
}

#[derive(Serialize)]
struct Header<'a> {
    schema: u32,
    dialect: &'static str,
    command: &'a str,
    version: &'static str,
}

/// The first record of every answer.
pub(crate) fn header(w: &mut dyn Write, command: &str) -> Result<()> {
    put(
        w,
        "greeg",
        &Header {
            schema: SCHEMA,
            dialect: "greeg",
            command,
            version: env!("CARGO_PKG_VERSION"),
        },
    )
}

/// The `outcome` of a footer. `exit` is the status the run returns.
#[derive(Serialize)]
pub(crate) struct OutcomeRec {
    exit: i32,
    exact: bool,
    rung: &'static str,
    total: usize,
    shown: usize,
    complete: bool,
    source: &'static str,
    fresh: &'static str,
    deferred: usize,
    truncated_by: Option<&'static str>,
}

impl OutcomeRec {
    pub(crate) fn of(o: &Outcome) -> OutcomeRec {
        OutcomeRec {
            exit: o.exit_code(),
            exact: o.exact(),
            rung: o.rung.name(),
            total: o.total,
            shown: o.shown,
            complete: o.complete(),
            source: o.source,
            fresh: o.fresh,
            deferred: o.deferred,
            truncated_by: o.truncated_by(),
        }
    }

    /// For a command that answers a file or a location: it exits 0 whenever it
    /// answers, even with nothing to show.
    /// `complete` overrides the counts' verdict: `show` is incomplete when
    /// it clips a body.
    pub(crate) fn answered(o: &Outcome, complete: bool) -> OutcomeRec {
        OutcomeRec {
            exit: 0,
            exact: true,
            rung: "exact",
            complete,
            truncated_by: Outcome {
                shown: if complete { o.total } else { 0 },
                ..o.clone()
            }
            .truncated_by(),
            ..OutcomeRec::of(o)
        }
    }
}

// ---- search ----

#[derive(Serialize)]
struct Facets<'a> {
    total: usize,
    files: usize,
    by_kind: Vec<(&'static str, usize)>,
    by_dir: &'a [(String, usize)],
    by_lang: &'a [(String, usize)],
    by_flag: &'a [(&'static str, usize)],
    definitions_total: usize,
    demoted_definitions: &'a [(&'static str, usize)],
    imported_by: Vec<Text<'a>>,
}

#[derive(Serialize)]
struct Begin<'a> {
    path: Text<'a>,
    encoding: &'static str,
    coordinates: &'static str,
    file_flags: Vec<&'static str>,
    matched_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_offset: Option<u64>,
}

#[derive(Serialize)]
struct SymbolRec<'a> {
    name: &'a str,
    kind: &'static str,
    container: String,
}

#[derive(Serialize)]
struct Match<'a> {
    line: u32,
    byte_offset: u32,
    text: Text<'a>,
    submatches: Vec<[u32; 2]>,
    kind: &'static str,
    symbol: Option<SymbolRec<'a>>,
    score: f64,
    clipped: bool,
}

#[derive(Serialize)]
struct Context<'a> {
    line: u32,
    byte_offset: u32,
    text: Text<'a>,
}

#[derive(Serialize)]
struct FileRec<'a> {
    path: Text<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_offset: Option<u64>,
}

#[derive(Serialize)]
struct SearchFooter<'a> {
    files_shown: usize,
    files_total: usize,
    demoted_files: usize,
    demoted_hits: usize,
    skipped_binary: usize,
    #[serde(skip_serializing_if = "is_zero")]
    binary_tails: usize,
    skipped_huge: usize,
    rung_names: &'a [String],
    ignored_only: Option<(usize, usize)>,
    ignored_partial: bool,
    est_tokens: usize,
    elapsed_ms: f64,
    hints: &'a [String],
    related: &'a [(String, usize)],
    layout: String,
    outcome: OutcomeRec,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Three decimals: scores and times carry no more meaning, and each digit
/// costs tokens.
fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// A line without its terminator (`\n` or `\r\n`).
fn line_text(b: &[u8]) -> &[u8] {
    let b = b.strip_suffix(b"\n").unwrap_or(b);
    b.strip_suffix(b"\r").unwrap_or(b)
}

fn file_records(w: &mut dyn Write, r: &ScanResult, sf: &ShownFile) -> Result<()> {
    let f: &FileResult = &r.files[sf.file];
    put(
        w,
        "begin",
        &Begin {
            path: Text::of(&f.rel),
            encoding: f.encoding.name(),
            coordinates: if f.encoding.transcoded() {
                "decoded"
            } else {
                "bytes"
            },
            file_flags: f.flags.names(),
            matched_lines: f.total,
            binary_offset: f.binary_offset,
        },
    )?;
    for line in file_lines(r, sf) {
        match line {
            FileLine::Match { hit, text } => {
                let h = &f.hits[hit];
                let symbol = h.chain.last().map(|(k, n)| SymbolRec {
                    name: n,
                    kind: k.name(),
                    container: chain_str(&h.chain[..h.chain.len() - 1]),
                });
                put(
                    w,
                    "match",
                    &Match {
                        line: h.line,
                        byte_offset: h.line_start,
                        text: Text::of(line_text(&text)),
                        submatches: h.raw_submatches().iter().map(|&(s, e)| [s, e]).collect(),
                        kind: h.kind.name(),
                        symbol,
                        score: r3(h.score as f64),
                        clipped: h.display(r.opts.max_columns).1,
                    },
                )?;
            }
            FileLine::Context { line, start, end } => {
                let b = &f.src.as_ref().expect("context comes from the source").bytes;
                put(
                    w,
                    "context",
                    &Context {
                        line,
                        byte_offset: start as u32,
                        text: Text::of(line_text(&b[start..end])),
                    },
                )?;
            }
        }
    }
    Ok(())
}

pub(crate) fn search(w: &mut dyn Write, r: &ScanResult, rep: &Report) -> Result<()> {
    header(w, "search")?;
    if let Some(fc) = &rep.facets {
        put(
            w,
            "facets",
            &Facets {
                total: r.stats.total_hits,
                files: r.stats.files_matched,
                by_kind: fc.by_kind.iter().map(|(k, n)| (k.name(), *n)).collect(),
                by_dir: &fc.by_dir,
                by_lang: &fc.by_lang,
                by_flag: &fc.by_flag,
                definitions_total: fc.defs_total,
                demoted_definitions: &fc.demoted_defs,
                imported_by: fc
                    .import_files
                    .iter()
                    .map(|&fi| Text::of(&r.files[fi].rel))
                    .collect(),
            },
        )?;
    }
    for sf in &rep.files {
        let f = &r.files[sf.file];
        match rep.layout {
            Layout::Files => put(
                w,
                "file",
                &FileRec {
                    path: Text::of(&f.rel),
                    count: None,
                    binary_offset: f.binary_offset,
                },
            )?,
            Layout::Count => put(
                w,
                "file",
                &FileRec {
                    path: Text::of(&f.rel),
                    count: Some(f.total),
                    binary_offset: f.binary_offset,
                },
            )?,
            _ => file_records(w, r, sf)?,
        }
    }
    let ft = &rep.footer;
    let rung_names: &[String] = match &ft.rung {
        greeg_query::Rung::SplitTokens(v) | greeg_query::Rung::Fuzzy(v) => v,
        _ => &[],
    };
    put(
        w,
        "footer",
        &SearchFooter {
            files_shown: ft.files_shown,
            files_total: ft.files_total,
            demoted_files: ft.demoted_files,
            demoted_hits: ft.demoted_hits,
            skipped_binary: ft.skipped_binary,
            binary_tails: ft.binary_tails,
            skipped_huge: ft.skipped_huge,
            rung_names,
            ignored_only: ft.ignored_only,
            ignored_partial: ft.ignored_partial,
            est_tokens: ft.est_tokens,
            elapsed_ms: r3(ft.elapsed_ms),
            hints: &ft.hints,
            related: &rep.related,
            layout: format!("{:?}", rep.layout).to_lowercase(),
            outcome: OutcomeRec::of(&Outcome::of_answer(r, ft)),
        },
    )
}

// ---- verbs ----

#[derive(Serialize)]
struct Def<'a> {
    path: Text<'a>,
    line: u32,
    kind: &'static str,
    name: &'a str,
    container: String,
    signature: &'a str,
    doc: Option<&'a str>,
    flags: Vec<&'static str>,
    supertypes: &'a [String],
    score: f64,
    reach: f64,
    start: u32,
    end: u32,
}

fn def(e: &DefEntry) -> Def<'_> {
    Def {
        path: Text::of(&e.rel),
        line: e.line,
        kind: e.kind.name(),
        name: &e.name,
        container: chain_str(&e.chain),
        signature: &e.signature,
        doc: e.doc.as_deref(),
        flags: crate::verbs_out::sym_flags(e.flags, e.file_flags),
        supertypes: &e.supers,
        score: r3(e.score as f64),
        reach: r3(e.reach as f64),
        start: e.start,
        end: e.end,
    }
}

#[derive(Serialize)]
struct DefFooter<'a> {
    name: &'a str,
    suggestions: &'a [String],
    suggestions_total: usize,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

/// The first `shown` entries and `suggestions` near names.
pub(crate) fn defs(
    w: &mut dyn Write,
    r: &verbs::DefResult,
    shown: usize,
    suggestions: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "def")?;
    for e in &r.entries[..shown] {
        put(w, "def", &def(e))?;
    }
    put(
        w,
        "footer",
        &DefFooter {
            name: &r.name,
            suggestions: &r.suggestions[..suggestions],
            suggestions_total: r.suggestions.len(),
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::of(oc),
        },
    )
}

#[derive(Serialize)]
struct Impl<'a> {
    #[serde(flatten)]
    def: Def<'a>,
    confidence: &'static str,
}

#[derive(Serialize)]
struct ImplsFooter<'a> {
    name: &'a str,
    direct: usize,
    extras: usize,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

pub(crate) fn impls(
    w: &mut dyn Write,
    r: &verbs::ImplsResult,
    direct: usize,
    extras: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "impls")?;
    let direct = r.direct.iter().take(direct).map(|e| (e, "high"));
    let extras = r.extras.iter().take(extras).map(|e| (e, "low"));
    for (e, confidence) in direct.chain(extras) {
        put(
            w,
            "impl",
            &Impl {
                def: def(e),
                confidence,
            },
        )?;
    }
    put(
        w,
        "footer",
        &ImplsFooter {
            name: &r.name,
            direct: r.direct_total,
            extras: r.extras_total,
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::of(oc),
        },
    )
}

#[derive(Serialize)]
struct Ref<'a> {
    kind: &'static str,
    path: Text<'a>,
    line: u32,
    text: Text<'a>,
    symbol: String,
    file_flags: Vec<&'static str>,
    score: f64,
}

#[derive(Serialize)]
struct RefsFooter<'a> {
    name: &'a str,
    definitions_total: usize,
    files_total: usize,
    by_kind: Vec<(&'static str, usize)>,
    resolved: usize,
    classified: usize,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

/// The first `defs` definitions; `shown` holds the hits shown, by file and
/// hit index, in order; `by_kind` the count of every kind with hits.
pub(crate) fn refs(
    w: &mut dyn Write,
    name: &str,
    r: &verbs::RefsResult,
    defs: usize,
    shown: &[(HitKind, usize, usize)],
    by_kind: Vec<(&'static str, usize)>,
    oc: &Outcome,
) -> Result<()> {
    let s = &r.scan;
    header(w, "refs")?;
    for e in &r.defs[..defs] {
        put(w, "def", &def(e))?;
    }
    for &(k, fi, hi) in shown {
        let f = &s.files[fi];
        let h = &f.hits[hi];
        put(
            w,
            "ref",
            &Ref {
                kind: k.name(),
                path: Text::of(&f.rel),
                line: h.line,
                text: Text::of(&h.display(s.opts.max_columns).0),
                symbol: chain_str(&h.chain),
                file_flags: f.flags.names(),
                score: r3(h.score as f64),
            },
        )?;
    }
    put(
        w,
        "footer",
        &RefsFooter {
            name,
            definitions_total: r.defs_total,
            files_total: s.stats.files_matched,
            by_kind,
            resolved: r.resolved,
            classified: r.classified,
            elapsed_ms: r3(s.stats.elapsed_ms),
            outcome: OutcomeRec::of(oc),
        },
    )
}

#[derive(Serialize)]
struct CallerRec<'a> {
    path: Text<'a>,
    symbol: String,
    kind: Option<&'static str>,
    def_line: u32,
    count: usize,
    lines: &'a [u32],
    file_flags: Vec<&'static str>,
    called_by: &'a [String],
}

fn caller(cl: &verbs::Caller) -> CallerRec<'_> {
    CallerRec {
        path: Text::of(&cl.rel),
        symbol: chain_str(&cl.chain),
        kind: cl.chain.last().map(|(k, _)| k.name()),
        def_line: cl.def_line,
        count: cl.count,
        lines: &cl.lines,
        file_flags: cl.file_flags.names(),
        called_by: &cl.called_by,
    }
}

#[derive(Serialize)]
struct CallersFooter<'a> {
    name: &'a str,
    call_sites: usize,
    files: usize,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

pub(crate) fn callers(
    w: &mut dyn Write,
    r: &verbs::CallersResult,
    limit: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "callers")?;
    for cl in r.callers.iter().take(limit) {
        put(w, "caller", &caller(cl))?;
    }
    put(
        w,
        "footer",
        &CallersFooter {
            name: &r.name,
            call_sites: r.total_hits,
            files: r.files,
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::of(oc),
        },
    )
}

#[derive(Serialize)]
struct Show<'a> {
    path: Text<'a>,
    line: u32,
    symbol: Option<SymbolRec<'a>>,
    start_line: u32,
    end_line: u32,
    shown_to: u32,
    clipped: bool,
    text: Text<'a>,
}

#[derive(Serialize)]
struct PlainFooter {
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

/// `bodies` holds each item's body as shown.
pub(crate) fn show(
    w: &mut dyn Write,
    r: &verbs::ShowResult,
    bodies: &[verbs::Body],
    oc: &Outcome,
) -> Result<()> {
    header(w, "show")?;
    let texts: Vec<Vec<u8>> = bodies.iter().map(|b| b.lines.join(&b'\n')).collect();
    for ((it, body), text) in r.items.iter().zip(bodies).zip(&texts) {
        let symbol = it.def.as_ref().map(|d| SymbolRec {
            name: &d.name,
            kind: d.kind.name(),
            container: chain_str(&d.chain[..d.chain.len().saturating_sub(1)]),
        });
        put(
            w,
            "show",
            &Show {
                path: Text::of(&it.rel),
                line: it.asked,
                symbol,
                start_line: body.first,
                end_line: body.last,
                shown_to: body.first + body.lines.len().saturating_sub(1) as u32,
                clipped: body.clipped,
                text: Text::of(text),
            },
        )?;
    }
    let complete = !bodies.iter().any(|b| b.clipped);
    put(
        w,
        "footer",
        &PlainFooter {
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::answered(oc, complete),
        },
    )
}

#[derive(Serialize)]
struct Symbol<'a> {
    name: &'a str,
    kind: &'static str,
    line: u32,
    start: u32,
    end: u32,
    container: String,
    depth: usize,
    flags: Vec<&'static str>,
}

#[derive(Serialize)]
struct OutlineFooter<'a> {
    path: Text<'a>,
    imports: &'a [String],
    parse_errors: bool,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

/// The first `shown` symbols.
pub(crate) fn outline(
    w: &mut dyn Write,
    r: &verbs::OutlineResult,
    shown: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "outline")?;
    for d in &r.defs[..shown] {
        put(
            w,
            "symbol",
            &Symbol {
                name: &d.name,
                kind: d.kind.name(),
                line: d.line,
                start: d.start,
                end: d.end,
                container: chain_str(&d.chain[..d.chain.len().saturating_sub(1)]),
                depth: d.chain.len().saturating_sub(1),
                flags: crate::verbs_out::sym_flags(d.flags, Default::default()),
            },
        )?;
    }
    put(
        w,
        "footer",
        &OutlineFooter {
            path: Text::of(&r.rel),
            imports: &r.imports,
            parse_errors: r.parse_errors,
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::answered(oc, shown == r.defs.len()),
        },
    )
}

#[derive(Serialize)]
struct Dir<'a> {
    path: Text<'a>,
    files: usize,
    symbols: usize,
    rank: f64,
}

#[derive(Serialize)]
struct MapFileRec<'a> {
    path: Text<'a>,
    rank: f64,
    symbols: usize,
    imported_by: usize,
    by_kind: Vec<(&'static str, usize)>,
    top: Vec<(&'static str, &'a str)>,
    file_flags: Vec<&'static str>,
}

#[derive(Serialize)]
struct MapFooter<'a> {
    dir: &'a str,
    files_total: usize,
    symbols_total: usize,
    dirs_total: usize,
    graph_changes: u32,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

pub(crate) fn map(
    w: &mut dyn Write,
    r: &verbs::MapResult,
    dir_limit: usize,
    file_limit: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "map")?;
    let dirs = &r.dirs[..r.dirs.len().min(dir_limit)];
    let files = &r.files[..r.files.len().min(file_limit)];
    for d in dirs {
        put(
            w,
            "dir",
            &Dir {
                path: Text::of(&d.rel),
                files: d.files,
                symbols: d.symbols,
                rank: r3(d.rank as f64),
            },
        )?;
    }
    for f in files {
        put(
            w,
            "file",
            &MapFileRec {
                path: Text::of(&f.rel),
                rank: r3(f.rank as f64),
                symbols: f.symbols,
                imported_by: f.imported_by,
                by_kind: f.by_kind.iter().map(|(k, n)| (k.name(), *n)).collect(),
                top: f.top.iter().map(|(k, n)| (k.name(), n.as_str())).collect(),
                file_flags: f.flags.names(),
            },
        )?;
    }
    let total = r.dirs.len() + r.files.len();
    let shown = dirs.len() + files.len();
    put(
        w,
        "footer",
        &MapFooter {
            dir: &r.dir,
            files_total: r.files_total,
            symbols_total: r.symbols_total,
            dirs_total: r.dirs.len(),
            graph_changes: r.graph_changes,
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::answered(oc, shown >= total),
        },
    )
}

#[derive(Serialize)]
struct Rebuilding<'a> {
    exit: i32,
    rebuilding: RebuildingRec<'a>,
}

#[derive(Serialize)]
struct RebuildingRec<'a> {
    reason: &'a str,
    estimate_ms: Option<u64>,
}

#[derive(Serialize)]
struct ErrorFooter<'a> {
    outcome: Rebuilding<'a>,
}

/// `map` while the index it needs is being rebuilt: exit 2.
pub(crate) fn map_rebuilding(
    w: &mut dyn Write,
    reason: &str,
    estimate_ms: Option<u64>,
) -> Result<()> {
    header(w, "map")?;
    put(
        w,
        "footer",
        &ErrorFooter {
            outcome: Rebuilding {
                exit: 2,
                rebuilding: RebuildingRec {
                    reason,
                    estimate_ms,
                },
            },
        },
    )
}

#[derive(Serialize)]
struct ImpactFileRec<'a> {
    path: Text<'a>,
    hits: usize,
    kinds: Vec<(&'static str, usize)>,
    file_flags: Vec<&'static str>,
    sample: Vec<(u32, Text<'a>)>,
}

fn impact_files(files: &[verbs::ImpactFile]) -> Vec<ImpactFileRec<'_>> {
    files
        .iter()
        .map(|f| ImpactFileRec {
            path: Text::of(&f.rel),
            hits: f.hits,
            kinds: f.kinds.iter().map(|(k, n)| (k.name(), *n)).collect(),
            file_flags: f.flags.names(),
            sample: f.sample.iter().map(|(l, t)| (*l, Text::of(t))).collect(),
        })
        .collect()
}

#[derive(Serialize)]
struct Impact<'a> {
    name: &'a str,
    definitions: Vec<Def<'a>>,
    likely: Vec<ImpactFileRec<'a>>,
    possible: Vec<ImpactFileRec<'a>>,
    review: Vec<ImpactFileRec<'a>>,
    import_graph: bool,
    callers: Vec<CallerRec<'a>>,
}

#[derive(Serialize)]
struct ImpactFooter<'a> {
    name: &'a str,
    definitions_total: usize,
    files: usize,
    callers_total: usize,
    total_hits: usize,
    elapsed_ms: f64,
    outcome: OutcomeRec,
}

/// The first `defs` definitions, `caps` files of each group (likely,
/// possible, review) and `callers` callers.
pub(crate) fn impact(
    w: &mut dyn Write,
    r: &verbs::ImpactResult,
    defs: usize,
    caps: [usize; 3],
    callers: usize,
    files: usize,
    oc: &Outcome,
) -> Result<()> {
    header(w, "impact")?;
    let cut = |g: &[verbs::ImpactFile], cap: usize| g.len().min(cap);
    put(
        w,
        "impact",
        &Impact {
            name: &r.name,
            definitions: r.defs[..defs].iter().map(def).collect(),
            likely: impact_files(&r.likely[..cut(&r.likely, caps[0])]),
            possible: impact_files(&r.possible[..cut(&r.possible, caps[1])]),
            review: impact_files(&r.review[..cut(&r.review, caps[2])]),
            import_graph: r.import_graph,
            callers: r.callers.callers.iter().take(callers).map(caller).collect(),
        },
    )?;
    put(
        w,
        "footer",
        &ImpactFooter {
            name: &r.name,
            definitions_total: r.defs_total,
            files,
            callers_total: r.callers.callers.len(),
            total_hits: r.total_hits,
            elapsed_ms: r3(r.elapsed_ms),
            outcome: OutcomeRec::of(oc),
        },
    )
}
