//! Text and JSON rendering for the symbol verbs (ARCHITECTURE.md).
//! Every text layout groups entries by file: the path once as a header, then
//! `  <line> <kind> <text>` rows beneath it.

use crate::{Common, chain_str, container_of, fmt_n};
use anyhow::Result;
use greeg_lang::sym::{SYM_EXPORTED, SYM_HAS_DOC, SYM_TEST};
use greeg_lang::{DefKind, FileFlags};
use greeg_query::outcome::Outcome;
use greeg_query::verbs::{self, DefEntry};
use greeg_query::{HitKind, Options};
use serde_json::json;
use std::io::{BufWriter, Write};

fn out() -> BufWriter<crate::stats::Tee<std::io::StdoutLock<'static>>> {
    BufWriter::with_capacity(64 * 1024, crate::stats::Tee(std::io::stdout().lock()))
}

/// ` · N ms` only with `--stats`.
fn ms(c: &Common, elapsed: f64) -> String {
    if c.stats {
        format!(" · {elapsed:.0} ms")
    } else {
        String::new()
    }
}

/// The `outcome` object of a JSON footer.
pub(crate) fn outcome_json(o: &Outcome) -> serde_json::Value {
    json!({"exit":o.exit_code(),"exact":o.exact(),"rung":o.rung.name(),"total":o.total,"shown":o.shown,
        "complete":o.complete(),"source":o.source,"fresh":o.fresh,"deferred":o.deferred})
}

/// End an answer in any format: flush it, record the run, and exit with the
/// outcome's status.
pub(crate) fn finish(mut w: impl Write, verb: &'static str, o: &Outcome) -> Result<()> {
    w.flush()?;
    finish_run(
        crate::stats::RunInfo {
            verb,
            hits: Some(o.total),
            source: Some(o.source.to_string()),
            ..Default::default()
        },
        o,
    )
}

/// Record the run with the outcome's status; exit when it is not 0.
pub(crate) fn finish_run(mut info: crate::stats::RunInfo, o: &Outcome) -> Result<()> {
    let exit = o.exit_code();
    info.exit = exit;
    crate::stats::record_run(info);
    if exit != 0 {
        greeg_query::indexed::flush_pending_build();
        std::process::exit(exit);
    }
    Ok(())
}

fn relaxed_note(rung: &greeg_query::Rung) -> String {
    if *rung != greeg_query::Rung::Exact {
        format!(" · matched {}", rung.describe())
    } else {
        String::new()
    }
}

fn parse_def_kind(s: &str) -> Result<DefKind> {
    Ok(match s {
        "fn" | "function" => DefKind::Function,
        "method" => DefKind::Method,
        "class" => DefKind::Class,
        "struct" => DefKind::Struct,
        "enum" => DefKind::Enum,
        "trait" => DefKind::Trait,
        "interface" => DefKind::Interface,
        "type" | "typealias" => DefKind::TypeAlias,
        "mod" | "module" | "namespace" => DefKind::Module,
        "object" => DefKind::Object,
        "impl" => DefKind::Impl,
        "const" | "constant" => DefKind::Constant,
        "var" | "variable" => DefKind::Variable,
        "macro" => DefKind::Macro,
        "field" | "property" => DefKind::Field,
        "variant" | "enum_member" => DefKind::Variant,
        other => anyhow::bail!("unknown --def-kind {other:?}"),
    })
}

fn sym_flags(flags: u8, file_flags: FileFlags) -> Vec<&'static str> {
    let mut v = Vec::new();
    if flags & SYM_EXPORTED != 0 {
        v.push("exported");
    }
    if flags & SYM_TEST != 0 || file_flags.has(FileFlags::TEST) {
        v.push("test");
    }
    if file_flags.has(FileFlags::GENERATED) {
        v.push("generated");
    }
    if file_flags.has(FileFlags::VENDORED) {
        v.push("vendored");
    }
    v
}

fn file_flag_suffix(rel: &str, flags: FileFlags) -> String {
    let mut names = flags.names();
    names.retain(|x| *x != "huge");
    if names.is_empty() && greeg_query::is_mock_path(rel) {
        names.push("mock");
    }
    if names.is_empty() {
        String::new()
    } else {
        format!("  [{}]", names.join(","))
    }
}

fn def_json(e: &DefEntry) -> serde_json::Value {
    json!({
        "path": e.rel, "line": e.line, "kind": e.kind.name(), "name": e.name, "container": chain_str(&e.chain),
        "signature": e.signature, "doc": e.doc, "flags": sym_flags(e.flags, e.file_flags), "supertypes": e.supers,
        "score": e.score, "reach": e.reach, "start": e.start, "end": e.end
    })
}

fn digits(n: u32) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// Definition entries grouped by file in order of first appearance:
/// `  <line> <kind>  [name  ]container › signature   [flags] : supers [reach]`.
/// A body beneath its row, dedented by its first line's indentation as the
/// block layout prints it (`  NNN  text`), then what was cut.
fn write_body(w: &mut impl Write, b: &verbs::Body, lw: usize) -> Result<()> {
    let lead = |l: &[u8]| {
        l.iter()
            .position(|c| !matches!(c, b' ' | b'\t'))
            .unwrap_or(l.len())
    };
    let indent = b.lines.first().map(|l| lead(l)).unwrap_or(0);
    for (i, l) in b.lines.iter().enumerate() {
        let cut = lead(l).min(indent);
        writeln!(
            w,
            "  {:>lw$}  {}",
            b.first + i as u32,
            String::from_utf8_lossy(&l[cut..]).trim_end()
        )?;
    }
    let shown_to = b.first + b.lines.len().saturating_sub(1) as u32;
    if b.clipped {
        writeln!(
            w,
            "  … {} more lines to {} (raise --budget)",
            b.last - shown_to,
            b.last
        )?;
    } else if b.last > shown_to {
        writeln!(
            w,
            "  … {} more lines to {} (--budget 0 lifts the {}-line cap)",
            b.last - shown_to,
            b.last,
            verbs::BODY_CAP
        )?;
    }
    Ok(())
}

/// `bodies`, when given, runs parallel to `entries` (`def --mode block`).
fn write_def_groups(
    w: &mut impl Write,
    entries: &[&DefEntry],
    show_name: bool,
    show_reach: bool,
    full_chain: bool,
    bodies: Option<&[Option<verbs::Body>]>,
) -> Result<()> {
    let mut order: Vec<&str> = Vec::new();
    for e in entries {
        if !order.contains(&e.rel.as_str()) {
            order.push(&e.rel);
        }
    }
    for rel in order {
        let group: Vec<(usize, &DefEntry)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.rel == rel)
            .map(|(i, e)| (i, *e))
            .collect();
        writeln!(w, "{}{}", rel, file_flag_suffix(rel, group[0].1.file_flags))?;
        let lw = group
            .iter()
            .map(|(i, e)| {
                let body_last = bodies
                    .and_then(|bs| bs[*i].as_ref())
                    .map(|b| b.first + b.lines.len() as u32)
                    .unwrap_or(0);
                digits(e.line.max(body_last))
            })
            .max()
            .unwrap_or(1);
        let kw = group
            .iter()
            .map(|(_, e)| e.kind.name().len())
            .max()
            .unwrap_or(0);
        let file_test = group[0].1.file_flags.has(FileFlags::TEST);
        for (i, e) in group {
            let ch = container_of(&e.chain, false, full_chain);
            let sig = if e.signature.is_empty() {
                e.name.clone()
            } else {
                e.signature.clone()
            };
            let mut ann: Vec<String> = Vec::new();
            let mut fl: Vec<&str> = Vec::new();
            if e.flags & SYM_EXPORTED != 0 {
                fl.push("exported");
            }
            if e.flags & SYM_TEST != 0 && !file_test {
                fl.push("test");
            }
            if !fl.is_empty() {
                ann.push(format!("[{}]", fl.join(",")));
            }
            if !e.supers.is_empty() {
                ann.push(format!(": {}", e.supers.join(", ")));
            }
            if show_reach && e.reach >= 0.8 {
                ann.push(format!("reach {:.1}", e.reach));
            }
            let ann = if ann.is_empty() {
                String::new()
            } else {
                format!("  {}", ann.join(" "))
            };
            let name = if show_name {
                format!("{}  ", e.name)
            } else {
                String::new()
            };
            writeln!(
                w,
                "  {:>lw$} {:<kw$}  {}{}{}{}{}",
                e.line,
                e.kind.name(),
                name,
                ch,
                if ch.is_empty() { "" } else { " › " },
                sig,
                ann
            )?;
            if let Some(d) = &e.doc {
                writeln!(w, "  {:lw$} {:kw$}  \"{d}\"", "", "")?;
            }
            if let Some(b) = bodies.and_then(|bs| bs[i].as_ref()) {
                write_body(w, b, lw)?;
            }
        }
    }
    Ok(())
}

pub fn run_def(
    c: &Common,
    o: &Options,
    name: &str,
    from: &[String],
    def_kind: Option<&str>,
) -> Result<()> {
    let want = def_kind.map(parse_def_kind).transpose()?;
    let explicit_from = !from.is_empty();
    let mut from: Vec<String> = from.to_vec();
    if from.is_empty()
        && !c.no_session
        && let Some(s) = greeg_query::session::Session::open(o, c.session.as_deref())
    {
        from = s.focus();
    }
    let r = verbs::def(o, name, &from, want)?;
    let oc = Outcome {
        total: r.total,
        shown: r.entries.len(),
        rung: r.rung.clone(),
        source: r.source,
        fresh: r.fresh,
        deferred: 0,
    };
    let mut w = out();
    if c.json {
        for e in &r.entries {
            let mut v = def_json(e);
            v["type"] = json!("def");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"def","name":r.name,"shown":r.entries.len(),"total":r.total,"source":r.source,"rung":r.rung.name(),"suggestions":r.suggestions,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
        )?;
        writeln!(w)?;
        return finish(w, "def", &oc);
    }
    if r.entries.is_empty() {
        writeln!(w, "def {}  no definition found ({})", r.name, r.source)?;
        if !r.suggestions.is_empty() {
            writeln!(w, "closest names: {}", r.suggestions.join(", "))?;
        } else {
            writeln!(w, "next: greeg {name} --kind def | greeg -i {name}")?;
        }
        return finish(w, "def", &oc);
    }
    let rung = relaxed_note(&r.rung);
    writeln!(
        w,
        "def {}  {} of {} definitions{} · {}{}",
        r.name,
        r.entries.len(),
        r.total,
        rung,
        r.source,
        ms(c, r.elapsed_ms)
    )?;
    let multi_name = r.entries.iter().any(|e| e.name != r.name);
    let entries: Vec<&DefEntry> = r.entries.iter().collect();
    // `--mode block`: each definition's body follows its row, the budget
    // shared in order (a module file has no body of its own)
    let bodies: Option<Vec<Option<verbs::Body>>> = if o.mode == greeg_query::Mode::Block {
        let mut est = 0usize;
        let mut sources: std::collections::HashMap<&str, greeg_query::Source> = Default::default();
        let mut v = Vec::with_capacity(entries.len());
        for e in &entries {
            if e.file_module {
                v.push(None);
                continue;
            }
            if !sources.contains_key(e.rel.as_str())
                && let Ok(src) = verbs::source_of(o, &e.rel)
            {
                sources.insert(e.rel.as_str(), src);
            }
            v.push(sources.get(e.rel.as_str()).map(|src| {
                let end = src.end_line(e.start, e.end);
                verbs::body(o, src, e.line, end, &mut est)
            }));
        }
        Some(v)
    } else {
        None
    };
    write_def_groups(
        &mut w,
        &entries,
        multi_name,
        explicit_from,
        c.chain,
        bodies.as_deref(),
    )?;
    if r.total > r.entries.len() {
        writeln!(w, "  +{} more (raise --budget)", r.total - r.entries.len())?;
    }
    if let Some(top) = r.entries.first() {
        writeln!(
            w,
            "next: refs {} | callers {} | outline {}",
            r.name, r.name, top.rel
        )?;
    }
    finish(w, "def", &oc)
}

pub fn run_show(c: &Common, o: &Options, locs: &[(String, u32)]) -> Result<()> {
    let r = verbs::show(o, locs)?;
    let mut w = out();
    if c.json {
        for it in &r.items {
            let shown_to = it.body.first + it.body.lines.len().saturating_sub(1) as u32;
            let text = it
                .body
                .lines
                .iter()
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            let symbol = it.def.as_ref().map(|d| {
                json!({"name":d.name,"kind":d.kind.name(),"container":chain_str(&d.chain[..d.chain.len().saturating_sub(1)])})
            });
            serde_json::to_writer(
                &mut w,
                &json!({"type":"show","data":{"path":it.rel,"line":it.asked,"symbol":symbol,"start_line":it.body.first,"end_line":it.body.last,"shown_to":shown_to,"clipped":it.body.clipped,"text":text}}),
            )?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"show","items":r.items.len(),"source":r.source,"elapsed_ms":r.elapsed_ms}}),
        )?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    for (n, it) in r.items.iter().enumerate() {
        let what = match &it.def {
            Some(d) => format!("{} {}", d.kind.name(), chain_str(&d.chain)),
            None => "no enclosing definition".to_string(),
        };
        writeln!(
            w,
            "show {}:{}  {} · lines {}–{} · {}{}",
            it.rel,
            it.asked,
            what,
            it.body.first,
            it.body.last,
            r.source,
            if n == 0 {
                ms(c, r.elapsed_ms)
            } else {
                String::new()
            }
        )?;
        let lw = digits(it.body.first + it.body.lines.len() as u32);
        write_body(&mut w, &it.body, lw)?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_refs(c: &Common, o: &Options, name: &str) -> Result<()> {
    let kinds: Vec<HitKind> = o.kinds.clone();
    let r = verbs::refs(o, name, &kinds)?;
    let s = &r.scan;
    let mut w = out();
    // group hits by kind, best first within each kind
    let mut by_kind: Vec<(HitKind, Vec<(usize, usize)>)> =
        HitKind::ALL.iter().map(|k| (*k, Vec::new())).collect();
    for (fi, f) in s.files.iter().enumerate() {
        for (hi, h) in f.hits.iter().enumerate() {
            by_kind[h.kind.idx()].1.push((fi, hi));
        }
    }
    for (_, v) in by_kind.iter_mut() {
        v.sort_by(|a, b| {
            s.files[b.0].hits[b.1]
                .score
                .partial_cmp(&s.files[a.0].hits[a.1].score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(s.files[a.0].rel.cmp(&s.files[b.0].rel))
                .then(
                    s.files[a.0].hits[a.1]
                        .line
                        .cmp(&s.files[b.0].hits[b.1].line),
                )
        });
    }
    let total_lines: usize = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 26).max(6)
    };
    // imports collapse to one line in text mode; other kinds share the budget by √count
    let collapse_imports = !c.json && o.kinds.is_empty();
    let nonempty: Vec<&(HitKind, Vec<(usize, usize)>)> = by_kind
        .iter()
        .filter(|(k, v)| !(v.is_empty() || collapse_imports && *k == HitKind::Import))
        .collect();
    let weight_sum: f64 = nonempty.iter().map(|(_, v)| (v.len() as f64).sqrt()).sum();
    let share_of = |n: usize| -> usize {
        if total_lines == usize::MAX {
            n
        } else {
            ((total_lines as f64 * (n as f64).sqrt() / weight_sum.max(1.0)).round() as usize)
                .clamp(n.min(2), n)
        }
    };
    let resolved = if let Some(pct) = (r.resolved * 100).checked_div(r.classified) {
        format!(" · {pct}% resolved")
    } else {
        String::new()
    };
    if c.json {
        for e in &r.defs {
            let mut v = def_json(e);
            v["type"] = json!("def");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        let mut shown = 0;
        for (k, v) in &nonempty {
            for &(fi, hi) in v.iter().take(share_of(v.len())) {
                shown += 1;
                let f = &s.files[fi];
                let h = &f.hits[hi];
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"ref","data":{"kind":k.name(),"path":f.rel,"line":h.line,"text":String::from_utf8_lossy(&h.text),"symbol":chain_str(&h.chain),"file_flags":f.flags.names(),"score":h.score}}),
                )?;
                writeln!(w)?;
            }
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"refs","name":name,"hits_total":s.stats.total_hits,"files_total":s.stats.files_matched,"by_kind":nonempty.iter().map(|(k,v)| json!([k.name(), v.len()])).collect::<Vec<_>>(),"resolved":r.resolved,"classified":r.classified,"rung":s.rung.name(),"source":s.stats.source,"elapsed_ms":s.stats.elapsed_ms,"outcome":outcome_json(&Outcome::of_search(s, shown))}}),
        )?;
        writeln!(w)?;
        return finish(w, "refs", &Outcome::of_search(s, shown));
    }
    if s.stats.total_hits == 0 {
        writeln!(w, "refs {name}  no references ({})", s.stats.source)?;
        return finish(w, "refs", &Outcome::of_search(s, 0));
    }
    let rung = relaxed_note(&s.rung);
    writeln!(
        w,
        "refs {}  {} hits · {} files{}{} · {}{}",
        name,
        fmt_n(s.stats.total_hits),
        fmt_n(s.stats.files_matched),
        resolved,
        rung,
        s.stats.source,
        ms(c, s.stats.elapsed_ms)
    )?;
    if !r.defs.is_empty() {
        let d: Vec<String> = r
            .defs
            .iter()
            .take(3)
            .map(|e| format!("{}:{} ({})", e.rel, e.line, e.kind.name()))
            .collect();
        writeln!(w, "defined at  {}", d.join("  "))?;
    }
    if collapse_imports && !by_kind[HitKind::Import.idx()].1.is_empty() {
        let mut files: Vec<usize> = Vec::new();
        for &(fi, _) in &by_kind[HitKind::Import.idx()].1 {
            if !files.contains(&fi) {
                files.push(fi);
            }
        }
        let names = crate::short_names(files.iter().take(6).map(|&fi| s.files[fi].rel.as_str()));
        let more = files.len().saturating_sub(6);
        writeln!(
            w,
            "imported by {} file{}: {}{}",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            names.join(", "),
            if more > 0 {
                format!(" (+{more})")
            } else {
                String::new()
            }
        )?;
    }
    let mut shown = 0usize;
    let fmt = crate::Fmt {
        chain: c.chain,
        stats: c.stats,
        line_numbers: false,
        stdin: false,
    };
    for (k, v) in &nonempty {
        let share = share_of(v.len());
        writeln!(w, "\n{} ({})", k.name(), fmt_n(v.len()))?;
        let mut seen_lines: Vec<(usize, u32)> = Vec::new();
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for &(fi, hi) in v.iter() {
            if taken.len() >= share {
                break;
            }
            let line = s.files[fi].hits[hi].line;
            if seen_lines.contains(&(fi, line)) {
                continue;
            }
            seen_lines.push((fi, line));
            taken.push((fi, hi));
        }
        shown += taken.len();
        crate::write_groups(&mut w, s, &taken, fmt, false)?;
        if v.len() > taken.len() {
            writeln!(w, "  +{} more", v.len() - taken.len())?;
        }
    }
    writeln!(
        w,
        "\n{}/{} hits · next: callers {name} | impact {name} | refs {name} --kind call",
        shown,
        fmt_n(s.stats.total_hits)
    )?;
    finish(w, "refs", &Outcome::of_search(s, shown))
}

pub fn run_callers(c: &Common, o: &Options, name: &str, depth: usize) -> Result<()> {
    let r = verbs::callers(o, name, depth)?;
    let mut w = out();
    let limit = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 22).max(5)
    };
    // an answer counts calling functions
    let oc = Outcome {
        total: r.callers.len(),
        shown: r.callers.len().min(limit),
        ..r.outcome.clone()
    };
    if c.json {
        for cl in r.callers.iter().take(limit) {
            serde_json::to_writer(
                &mut w,
                &json!({"type":"caller","data":{"path":cl.rel,"symbol":chain_str(&cl.chain),"kind":cl.chain.last().map(|(k,_)| k.name()),"def_line":cl.def_line,"count":cl.count,"lines":cl.lines,"file_flags":cl.file_flags.names(),"called_by":cl.called_by}}),
            )?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"callers","name":r.name,"callers":r.callers.len(),"call_sites":r.total_hits,"files":r.files,"source":r.source,"rung":r.rung.name(),"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
        )?;
        writeln!(w)?;
        return finish(w, "callers", &oc);
    }
    if r.callers.is_empty() {
        writeln!(w, "callers {}  no call sites ({})", r.name, r.source)?;
        return finish(w, "callers", &oc);
    }
    writeln!(
        w,
        "callers {}  {} call sites in {} functions · {} files{} · {}{}",
        r.name,
        fmt_n(r.total_hits),
        fmt_n(r.callers.len()),
        fmt_n(r.files),
        relaxed_note(&r.rung),
        r.source,
        ms(c, r.elapsed_ms)
    )?;
    let shown: Vec<&verbs::Caller> = r.callers.iter().take(limit).collect();
    let mut order: Vec<&str> = Vec::new();
    for cl in &shown {
        if !order.contains(&cl.rel.as_str()) {
            order.push(&cl.rel);
        }
    }
    for rel in order {
        let group: Vec<&verbs::Caller> = shown.iter().filter(|cl| cl.rel == rel).copied().collect();
        writeln!(w, "{}{}", rel, file_flag_suffix(rel, group[0].file_flags))?;
        let lw = group
            .iter()
            .map(|cl| digits(cl.def_line))
            .max()
            .unwrap_or(1);
        let kw = group
            .iter()
            .map(|cl| cl.chain.last().map(|(k, _)| k.name().len()).unwrap_or(0))
            .max()
            .unwrap_or(0);
        for cl in group {
            let sym = if cl.chain.is_empty() {
                "(top level)".to_string()
            } else {
                container_of(&cl.chain, false, c.chain)
            };
            let kind = cl.chain.last().map(|(k, _)| k.name()).unwrap_or("");
            let lines: Vec<String> = cl.lines.iter().map(|l| l.to_string()).collect();
            writeln!(
                w,
                "  {:>lw$} {:<kw$}  {} ×{} (lines {})",
                cl.def_line,
                kind,
                sym,
                cl.count,
                lines.join(", ")
            )?;
            if !cl.called_by.is_empty() {
                writeln!(
                    w,
                    "  {:lw$} {:kw$}  ← called by {}",
                    "",
                    "",
                    cl.called_by.join(", ")
                )?;
            }
        }
    }
    if r.callers.len() > limit {
        writeln!(
            w,
            "  +{} more callers (raise --budget)",
            r.callers.len() - limit
        )?;
    }
    finish(w, "callers", &oc)
}

pub fn run_impls(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = verbs::impls(o, name)?;
    let mut w = out();
    let limit = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 30).max(5)
    };
    let oc = Outcome {
        total: r.direct_total + r.extras_total,
        shown: r.direct.len().min(limit)
            + r.extras
                .len()
                .min(if c.json { limit } else { limit / 2 + 1 }),
        rung: greeg_query::Rung::Exact,
        source: r.source,
        fresh: r.fresh,
        deferred: 0,
    };
    if c.json {
        for e in r.direct.iter().take(limit) {
            let mut v = def_json(e);
            v["type"] = json!("impl");
            v["confidence"] = json!("high");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        for e in r.extras.iter().take(limit) {
            let mut v = def_json(e);
            v["type"] = json!("impl");
            v["confidence"] = json!("low");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"impls","name":r.name,"direct":r.direct_total,"extras":r.extras_total,"source":r.source,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
        )?;
        writeln!(w)?;
        return finish(w, "impls", &oc);
    }
    if r.direct.is_empty() && r.extras.is_empty() {
        writeln!(w, "impls {}  none found ({})", r.name, r.source)?;
        return finish(w, "impls", &oc);
    }
    writeln!(
        w,
        "impls {}  {} implementations{} · {}{}",
        r.name,
        r.direct_total,
        if r.extras.is_empty() {
            String::new()
        } else {
            format!(" + {} low-confidence", r.extras_total)
        },
        r.source,
        ms(c, r.elapsed_ms)
    )?;
    let direct: Vec<&DefEntry> = r.direct.iter().take(limit).collect();
    write_def_groups(&mut w, &direct, true, false, c.chain, None)?;
    if r.direct_total > direct.len() {
        writeln!(w, "  +{} more", r.direct_total - direct.len())?;
    }
    if !r.extras.is_empty() {
        writeln!(
            w,
            "low confidence ({} in type position on definition lines)",
            r.name
        )?;
        let extras: Vec<&DefEntry> = r.extras.iter().take(limit / 2 + 1).collect();
        write_def_groups(&mut w, &extras, true, false, c.chain, None)?;
    }
    finish(w, "impls", &oc)
}

pub fn run_outline(c: &Common, o: &Options, file: &str, imports: bool) -> Result<()> {
    let r = verbs::outline(o, file)?;
    let mut w = out();
    if c.json {
        for d in &r.defs {
            serde_json::to_writer(
                &mut w,
                &json!({"type":"symbol","data":{"path":r.rel,"name":d.name,"kind":d.kind.name(),"line":d.line,"start":d.start,"end":d.end,"container":chain_str(&d.chain[..d.chain.len().saturating_sub(1)]),"depth":d.chain.len().saturating_sub(1),"flags":sym_flags(d.flags, FileFlags::default())}}),
            )?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"outline","path":r.rel,"symbols":r.defs.len(),"imports":r.imports,"source":r.source,"parse_errors":r.parse_errors,"elapsed_ms":r.elapsed_ms}}),
        )?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    writeln!(
        w,
        "outline {}  {} symbols · {} imports · {} · {}{}",
        r.rel,
        r.defs.len(),
        r.imports.len(),
        r.lang.name(),
        r.source,
        ms(c, r.elapsed_ms)
    )?;
    if r.parse_errors {
        writeln!(
            w,
            "  (file has parse errors; some definitions may come from the regex fallback)"
        )?;
    }
    // a file of declarations only (a Rust `mod.rs` of `mod x;` lines) has
    // nothing else to show: its imports are the outline
    if !r.imports.is_empty() && (imports || r.imports.len() <= 3 || r.defs.is_empty()) {
        let imps: Vec<&str> = r.imports.iter().map(|s| s.as_str()).take(40).collect();
        writeln!(
            w,
            "  imports  {}{}",
            imps.join(", "),
            if r.imports.len() > 40 {
                format!(" +{}", r.imports.len() - 40)
            } else {
                String::new()
            }
        )?;
    }
    // budget: collapse depth when the tree is large
    let max_lines = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 12).max(10)
    };
    let mut max_depth = 8usize;
    while max_depth > 0
        && r.defs
            .iter()
            .filter(|d| d.chain.len() <= max_depth + 1)
            .count()
            > max_lines
    {
        max_depth -= 1;
    }
    let mut printed = 0usize;
    let mut hidden_counts: Vec<usize> = vec![0; r.defs.len()];
    for (i, d) in r.defs.iter().enumerate() {
        let depth = d.chain.len().saturating_sub(1);
        if depth > max_depth {
            // attribute to the nearest visible ancestor
            let mut j = i;
            while j > 0 {
                j -= 1;
                if r.defs[j].chain.len().saturating_sub(1) <= max_depth
                    && d.start >= r.defs[j].start
                    && d.end <= r.defs[j].end
                {
                    hidden_counts[j] += 1;
                    break;
                }
            }
            continue;
        }
        printed += 1;
    }
    let mut n = 0usize;
    for (i, d) in r.defs.iter().enumerate() {
        let depth = d.chain.len().saturating_sub(1);
        if depth > max_depth {
            continue;
        }
        n += 1;
        if n > max_lines {
            writeln!(
                w,
                "  +{} more symbols (raise --budget)",
                printed - max_lines
            )?;
            break;
        }
        let mut ann: Vec<&str> = Vec::new();
        if d.flags & SYM_EXPORTED != 0 && depth == 0 && d.kind != DefKind::Impl {
            ann.push("pub");
        }
        if d.flags & SYM_HAS_DOC != 0 {
            ann.push("doc");
        }
        if d.flags & SYM_TEST != 0 {
            ann.push("test");
        }
        let ann = if ann.is_empty() {
            String::new()
        } else {
            format!("  [{}]", ann.join(","))
        };
        let more = if hidden_counts[i] > 0 {
            format!("  (+{} nested)", hidden_counts[i])
        } else {
            String::new()
        };
        writeln!(
            w,
            "  {}{} {}  :{}{}{}",
            "  ".repeat(depth),
            d.kind.name(),
            d.name,
            d.line,
            ann,
            more
        )?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_map(c: &Common, o: &Options, dir: &str) -> Result<()> {
    let r = verbs::map(o, dir)?;
    let mut w = out();
    let file_limit = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 45).clamp(5, 60)
    };
    let dir_limit = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 60).clamp(4, 24)
    };
    if c.json {
        for d in r.dirs.iter().take(dir_limit) {
            serde_json::to_writer(
                &mut w,
                &json!({"type":"dir","data":{"path":d.rel,"files":d.files,"symbols":d.symbols,"rank":d.rank}}),
            )?;
            writeln!(w)?;
        }
        for f in r.files.iter().take(file_limit) {
            serde_json::to_writer(
                &mut w,
                &json!({"type":"file","data":{"path":f.rel,"rank":f.rank,"symbols":f.symbols,"imported_by":f.imported_by,"by_kind":f.by_kind.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"top":f.top.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names()}}),
            )?;
            writeln!(w)?;
        }
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"map","dir":r.dir,"files_total":r.files_total,"symbols_total":r.symbols_total,"dirs_total":r.dirs.len(),"source":r.source,"elapsed_ms":r.elapsed_ms}}),
        )?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    writeln!(
        w,
        "map {}  {} files · {} symbols · {} subdirectories · {}{}",
        if r.dir.is_empty() { "." } else { &r.dir },
        fmt_n(r.files_total),
        fmt_n(r.symbols_total),
        r.dirs.len(),
        r.source,
        ms(c, r.elapsed_ms)
    )?;
    if !r.dirs.is_empty() {
        writeln!(w, "\ndirectories (by best file rank)")?;
        for d in r.dirs.iter().take(dir_limit) {
            writeln!(
                w,
                "  {:<40} {:>5} files {:>7} symbols  rank {:.2}",
                format!("{}/", d.rel),
                d.files,
                fmt_n(d.symbols),
                d.rank
            )?;
        }
        if r.dirs.len() > dir_limit {
            writeln!(w, "  +{} more directories", r.dirs.len() - dir_limit)?;
        }
    }
    writeln!(w, "\nfiles (by import PageRank)")?;
    for f in r.files.iter().take(file_limit) {
        let kinds: Vec<String> = f
            .by_kind
            .iter()
            .map(|(k, n)| format!("{} {}", n, k.name()))
            .collect();
        let top: Vec<String> = f
            .top
            .iter()
            .map(|(k, n)| format!("{} {}", k.name(), n))
            .collect();
        writeln!(
            w,
            "  {}{}  rank {:.2} · ←{} · {}",
            f.rel,
            file_flag_suffix(&f.rel, f.flags),
            f.rank,
            f.imported_by,
            kinds.join(", ")
        )?;
        if !top.is_empty() {
            writeln!(w, "      {}", top.join(", "))?;
        }
    }
    if r.files.len() > file_limit {
        writeln!(
            w,
            "  +{} more files (raise --budget or narrow the directory)",
            r.files.len() - file_limit
        )?;
    }
    if let Some(d) = r.dirs.first() {
        writeln!(
            w,
            "next: map {} | outline {}",
            d.rel,
            r.files.first().map(|f| f.rel.as_str()).unwrap_or("")
        )?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_impact(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = verbs::impact(o, name)?;
    // an answer counts referring files
    let files = r.will_break.len() + r.may_break.len() + r.review.len();
    let oc = Outcome {
        total: files,
        shown: files,
        ..r.outcome.clone()
    };
    let mut w = out();
    let per_group = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 130).clamp(3, 30)
    };
    let write_group =
        |w: &mut dyn Write, title: &str, why: &str, files: &[verbs::ImpactFile]| -> Result<()> {
            if files.is_empty() {
                return Ok(());
            }
            let hits: usize = files.iter().map(|f| f.hits).sum();
            writeln!(
                w,
                "\n{} ({} files, {} hits) — {}",
                title,
                files.len(),
                fmt_n(hits),
                why
            )?;
            for f in files.iter().take(per_group) {
                let kinds: Vec<String> = f
                    .kinds
                    .iter()
                    .map(|(k, n)| format!("{} {}", k.name(), n))
                    .collect();
                writeln!(
                    w,
                    "{}{}  {}",
                    f.rel,
                    file_flag_suffix(&f.rel, f.flags),
                    kinds.join(", ")
                )?;
                let lw = f
                    .sample
                    .iter()
                    .take(2)
                    .map(|(l, _)| digits(*l))
                    .max()
                    .unwrap_or(1);
                for (l, t) in f.sample.iter().take(2) {
                    writeln!(w, "  {:>lw$}  {}", l, t.trim())?;
                }
            }
            if files.len() > per_group {
                writeln!(w, "  +{} more files", files.len() - per_group)?;
            }
            Ok(())
        };
    if c.json {
        let grp = |files: &[verbs::ImpactFile]| -> Vec<serde_json::Value> {
            files.iter().map(|f| json!({"path":f.rel,"hits":f.hits,"kinds":f.kinds.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names(),"sample":f.sample})).collect()
        };
        serde_json::to_writer(
            &mut w,
            &json!({"type":"impact","data":{"name":r.name,"definitions":r.defs.iter().map(def_json).collect::<Vec<_>>(),"will_break":grp(&r.will_break),"may_break":grp(&r.may_break),"review":grp(&r.review),
            "callers":r.callers.callers.iter().take(per_group).map(|cl| json!({"path":cl.rel,"symbol":chain_str(&cl.chain),"count":cl.count,"called_by":cl.called_by})).collect::<Vec<_>>(),
            "total_hits":r.total_hits,"elapsed_ms":r.elapsed_ms}}),
        )?;
        writeln!(w)?;
        serde_json::to_writer(
            &mut w,
            &json!({"type":"footer","data":{"verb":"impact","name":r.name,"files":files,"total_hits":r.total_hits,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
        )?;
        writeln!(w)?;
        return finish(w, "impact", &oc);
    }
    if r.total_hits == 0 {
        writeln!(w, "impact {}  no references found", r.name)?;
        return finish(w, "impact", &oc);
    }
    writeln!(
        w,
        "impact {}  {} hits · {} files · {} definitions{}{}",
        r.name,
        fmt_n(r.total_hits),
        files,
        r.defs.len(),
        relaxed_note(&r.outcome.rung),
        ms(c, r.elapsed_ms)
    )?;
    let defs: Vec<&DefEntry> = r.defs.iter().take(3).collect();
    write_def_groups(&mut w, &defs, false, false, c.chain, None)?;
    write_group(
        &mut w,
        "WILL BREAK",
        "calls, type uses or imports in source",
        &r.will_break,
    )?;
    write_group(
        &mut w,
        "MAY BREAK",
        "member or bare identifier uses",
        &r.may_break,
    )?;
    write_group(
        &mut w,
        "REVIEW",
        "tests, comments, strings, demoted files",
        &r.review,
    )?;
    if !r.callers.callers.is_empty() {
        writeln!(
            w,
            "\ncallers ({} functions, depth 2)",
            r.callers.callers.len()
        )?;
        let shown: Vec<&verbs::Caller> = r.callers.callers.iter().take(per_group).collect();
        let mut order: Vec<&str> = Vec::new();
        for cl in &shown {
            if !order.contains(&cl.rel.as_str()) {
                order.push(&cl.rel);
            }
        }
        for rel in order {
            writeln!(w, "{rel}")?;
            for cl in shown.iter().filter(|cl| cl.rel == rel) {
                let sym = if cl.chain.is_empty() {
                    "(top level)".to_string()
                } else {
                    container_of(&cl.chain, false, c.chain)
                };
                writeln!(
                    w,
                    "  {}  {} ×{}{}",
                    cl.def_line,
                    sym,
                    cl.count,
                    if cl.called_by.is_empty() {
                        String::new()
                    } else {
                        format!("  ← {}", cl.called_by.join(", "))
                    }
                )?;
            }
        }
    }
    finish(w, "impact", &oc)
}
