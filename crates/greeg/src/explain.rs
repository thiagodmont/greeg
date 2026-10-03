//! `--explain`: what a search's answer was built from. It is outside the
//! budget and the byte ceiling, and never recorded.

use crate::json_data;
use greeg_lang::FileFlags;
use greeg_query::{ScanResult, shape::Report};
use serde_json::{Value, json};
use std::io::Write;

/// Shown hits whose ranking terms are listed, in output order.
const RANKED: usize = 10;

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
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
    let ranked: Vec<Value> = rep
        .files
        .iter()
        .flat_map(|sf| sf.hits.iter().map(move |sh| (sf.file, sh.hit)))
        .take(RANKED)
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
        "index_skipped": (!s.index_skipped.is_empty()).then_some(s.index_skipped),
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

/// The record as stderr lines, for text answers.
pub fn write_text(w: &mut impl Write, e: &Value) -> std::io::Result<()> {
    let d = &e["data"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let mut source = format!("source {}", d["source"].as_str().unwrap_or(""));
    if let Some(why) = d["index_skipped"].as_str() {
        source += &format!(" (index not used: {why})");
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
        for h in hits {
            writeln!(
                w,
                "  {}:{} {} {} = {} × {} × {}",
                h["path"]["text"].as_str().unwrap_or("?"),
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
