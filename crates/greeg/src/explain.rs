//! `--explain`: what a search's or a command's answer was built from. It is
//! outside the budget and the byte ceiling, and never recorded.

use crate::json_data;
use greeg_lang::FileFlags;
use greeg_query::outcome::Outcome;
use greeg_query::verbs::{DefEntry, ScoreTerms};
use greeg_query::{ScanResult, shape::Report};
use serde_json::{Value, json};
use std::io::Write;

/// Shown hits whose ranking terms are listed, in output order.
const RANKED: usize = 10;

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The shown hits whose terms are listed: (file, hit) indexes.
fn ranked(rep: &Report) -> impl Iterator<Item = (usize, usize)> + '_ {
    rep.files
        .iter()
        .flat_map(|sf| sf.hits.iter().map(move |sh| (sf.file, sh.hit)))
        .take(RANKED)
}

/// The `explain` record.
pub fn search(r: &ScanResult, rep: &Report) -> Value {
    let (s, o) = (&r.stats, &r.opts);
    let index = s.source.starts_with("index");
    let excluded: Vec<&str> = [
        (o.no_tests, "tests"),
        (o.no_vendored, "vendored"),
        (o.no_generated, "generated"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let ranked: Vec<Value> = ranked(rep)
        .map(|(fi, hi)| {
            let (f, h) = (&r.files[fi], &r.files[fi].hits[hi]);
            json!({
                "path": json_data(&f.rel),
                "line": h.line,
                "kind": h.kind.name(),
                "score": r3(h.score.into()),
                "kind_weight": r3(h.kind.weight().into()),
                "exact_boost": r3(greeg_query::exact_boost(h.kind, h.exact).into()),
                "prior": r3(f.prior.into()),
            })
        })
        .collect();
    json!({"type": "explain", "data": {
        "source": s.source,
        "index_skipped": (!s.index_skipped.is_empty()).then_some(&s.index_skipped),
        "fresh": index.then(|| json!({
            "method": s.fresh_method,
            "ms": r3(s.fresh_ms),
            "changed": s.fresh_changed,
            "deferred": s.fresh_deferred,
        })),
        "plan": index.then_some(&s.plan),
        "rung": r.rung.name(),
        "candidates": {
            "walked": s.files_walked,
            "candidates": s.candidates,
            "searched": s.files_searched,
            "matched_files": s.files_matched,
            "hits_before_kind": s.total_unfiltered,
            "hits": s.total_hits,
        },
        "filters": {
            "kinds": o.kinds.iter().map(|k| k.name()).collect::<Vec<_>>(),
            "excluded": excluded,
            "globs": o.globs,
            "types": o.types,
            "types_not": o.types_not,
            "demoted": !o.all,
            "demoted_files": s.demoted_files,
            "demoted_hits": s.demoted_hits,
            "skipped_binary": s.skipped_binary,
            "binary_tails": s.binary_tails,
            "skipped_huge": s.skipped_huge,
            "ignored_only": r.ignored_only.map(|(files, hits)| json!({"files": files, "hits": hits})),
        },
        "ranking": {
            "terms": ["kind_weight", "exact_boost", "prior"],
            "hits": ranked,
        },
        "parse_errors": index.then(|| r.files.iter().filter(|f| f.flags.has(FileFlags::PARSE_ERRORS)).count()),
    }})
}

/// The record as stderr lines, for text answers; paths are escaped for a
/// terminal.
pub fn write_text(
    w: &mut impl Write,
    e: &Value,
    r: &ScanResult,
    rep: &Report,
) -> std::io::Result<()> {
    let d = &e["data"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let mut source = format!("source {}", d["source"].as_str().unwrap_or(""));
    match d["index_skipped"].as_str() {
        Some("not used") => source += " (index not used)",
        Some(why) => source += &format!(" (index not used: {why})"),
        None => {}
    }
    if d["fresh"].is_object() {
        let f = &d["fresh"];
        source += &format!(
            " · fresh {} {} ms, {} changed, {} deferred",
            f["method"].as_str().unwrap_or(""),
            f["ms"],
            n(&f["changed"]),
            n(&f["deferred"])
        );
    }
    writeln!(
        w,
        "explain: {source} · matched {}",
        d["rung"].as_str().unwrap_or("")
    )?;
    if let Some(plan) = d["plan"].as_str() {
        writeln!(w, "explain: plan {plan}")?;
    }
    let c = &d["candidates"];
    writeln!(
        w,
        "explain: walked {} · candidates {} · searched {} · matched {} files, {} hits ({} before --kind)",
        n(&c["walked"]),
        n(&c["candidates"]),
        n(&c["searched"]),
        n(&c["matched_files"]),
        n(&c["hits"]),
        n(&c["hits_before_kind"])
    )?;
    let f = &d["filters"];
    let list = |v: &Value| {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    let mut parts = Vec::new();
    for (key, label) in [
        ("kinds", "kind"),
        ("excluded", "excluded"),
        ("globs", "glob"),
        ("types", "type"),
        ("types_not", "type-not"),
    ] {
        let l = list(&f[key]);
        if !l.is_empty() {
            parts.push(format!("{label} {l}"));
        }
    }
    if f["demoted"] == true {
        parts.push(format!(
            "demoted {} files ({} hits)",
            n(&f["demoted_files"]),
            n(&f["demoted_hits"])
        ));
    }
    parts.push(format!(
        "skipped binary {}, binary tails {}, huge {}",
        n(&f["skipped_binary"]),
        n(&f["binary_tails"]),
        n(&f["skipped_huge"])
    ));
    if let Some(io) = f["ignored_only"].as_object() {
        parts.push(format!(
            "only in ignored files: {} files, {} hits",
            n(&io["files"]),
            n(&io["hits"])
        ));
    }
    writeln!(w, "explain: filters: {}", parts.join(" · "))?;
    if let Some(pe) = d["parse_errors"].as_u64() {
        writeln!(
            w,
            "explain: {pe} matched files parsed with errors or by the regex fallback"
        )?;
    }
    let hits = d["ranking"]["hits"].as_array().cloned().unwrap_or_default();
    if !hits.is_empty() {
        writeln!(
            w,
            "explain: score = kind weight × exact boost × prior (location, --near, PageRank)"
        )?;
        for (h, (fi, _)) in hits.iter().zip(ranked(rep)) {
            writeln!(
                w,
                "  {}:{} {} {} = {} × {} × {}",
                r.files[fi].rel_text(),
                h["line"],
                h["kind"].as_str().unwrap_or(""),
                h["score"],
                h["kind_weight"],
                h["exact_boost"],
                h["prior"]
            )?;
        }
    }
    Ok(())
}

/// What a command's `--explain` adds to its outcome.
pub struct VerbExplain<'a> {
    /// Counts of what the command considered, in the order shown.
    pub considered: Value,
    /// `def`: its definitions, best first.
    pub ranked: &'a [DefEntry],
}

impl<'a> VerbExplain<'a> {
    pub fn new(considered: Value) -> Self {
        VerbExplain {
            considered,
            ranked: &[],
        }
    }
}

fn terms_json(t: &ScoreTerms) -> Value {
    json!({
        "kind": r3(t.kind.into()),
        "exported": r3(t.exported.into()),
        "nested": r3(t.nested.into()),
        "location": r3(t.location.into()),
        "rank": r3(t.rank.into()),
        "reach": r3(t.reach.into()),
    })
}

/// The `explain` record of a command's answer; `fit` is the row allowance it
/// was rendered at and the limits that cut it.
pub fn verb(verb: &str, o: &Outcome, ex: &VerbExplain, fit: Value) -> Value {
    let ranking: Vec<Value> = ex
        .ranked
        .iter()
        .take(o.shown.min(RANKED))
        .map(|e| {
            json!({
                "path": json_data(&e.rel),
                "line": e.line,
                "kind": e.kind.name(),
                "score": r3(e.score.into()),
                "terms": e.terms.as_ref().map(terms_json),
            })
        })
        .collect();
    json!({"type": "explain", "data": {
        "verb": verb,
        "source": o.source,
        "fresh": (!o.fresh.is_empty()).then_some(o.fresh),
        "deferred": o.deferred,
        "rung": o.rung.name(),
        "total": o.total,
        "shown": o.shown,
        "truncated_by": o.truncated_by(),
        "fit": fit,
        "considered": ex.considered,
        "ranking": (!ranking.is_empty()).then(|| json!({
            "terms": ["kind", "exported", "nested", "location", "rank", "reach"],
            "definitions": ranking,
        })),
    }})
}

/// A command's record as stderr lines; paths are escaped for a terminal.
pub fn write_verb_text(w: &mut impl Write, e: &Value, ex: &VerbExplain) -> std::io::Result<()> {
    let d = &e["data"];
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let mut line = format!("explain: {} · source {}", s(&d["verb"]), s(&d["source"]));
    if let Some(f) = d["fresh"].as_str() {
        line += &format!(" · fresh {f}, {} deferred", d["deferred"]);
    }
    line += &format!(
        " · matched {} · {} of {} shown",
        s(&d["rung"]),
        d["shown"],
        d["total"]
    );
    if let Some(t) = d["truncated_by"].as_str() {
        line += &format!(" (cut by the {t})");
    }
    let by: Vec<&str> = d["fit"]["cut_by"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| match v.as_str()? {
            "bytes" => Some("byte ceiling"),
            other => Some(other),
        })
        .collect();
    if !by.is_empty() {
        line += &format!(
            " · fit {} of {} rows to the {}",
            d["fit"]["rows"],
            d["fit"]["of"],
            by.join(" and the ")
        );
    }
    writeln!(w, "{line}")?;
    if let Some(m) = d["considered"].as_object().filter(|m| !m.is_empty()) {
        let parts: Vec<String> = m
            .iter()
            .map(|(k, v)| format!("{} {v}", k.replace('_', " ")))
            .collect();
        writeln!(w, "explain: considered: {}", parts.join(" · "))?;
    }
    let ranked = d["ranking"]["definitions"].as_array();
    if let Some(defs) = ranked.filter(|r| !r.is_empty()) {
        writeln!(
            w,
            "explain: score = kind × exported × nested × location × rank (PageRank) × reach"
        )?;
        for (h, e) in defs.iter().zip(ex.ranked) {
            let t = &h["terms"];
            let terms = if t.is_object() {
                format!(
                    " = {} × {} × {} × {} × {} × {}",
                    t["kind"], t["exported"], t["nested"], t["location"], t["rank"], t["reach"]
                )
            } else {
                String::new()
            };
            writeln!(
                w,
                "  {}:{} {} {}{terms}",
                greeg_index::rel::display(&e.rel),
                h["line"],
                h["kind"].as_str().unwrap_or(""),
                h["score"]
            )?;
        }
    }
    Ok(())
}
