//! Text and JSON rendering for the symbol verbs (DESIGN.md §7.3–§7.4, OUTPUT.md).

use crate::{Common, chain_str, fmt_n, hit_chain};
use anyhow::Result;
use greeg_lang::sym::{SYM_EXPORTED, SYM_HAS_DOC, SYM_TEST};
use greeg_lang::{DefKind, FileFlags};
use greeg_query::verbs::{self, DefEntry};
use greeg_query::{HitKind, Options};
use serde_json::json;
use std::io::{BufWriter, Write};

fn out() -> BufWriter<std::io::StdoutLock<'static>> {
    BufWriter::with_capacity(64 * 1024, std::io::stdout().lock())
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

fn def_json(e: &DefEntry) -> serde_json::Value {
    json!({
        "path": e.rel, "line": e.line, "kind": e.kind.name(), "name": e.name, "container": chain_str(&e.chain),
        "signature": e.signature, "doc": e.doc, "flags": sym_flags(e.flags, e.file_flags), "supertypes": e.supers,
        "score": e.score, "reach": e.reach, "start": e.start, "end": e.end
    })
}

fn write_def_entry(w: &mut impl Write, e: &DefEntry, show_name: bool) -> Result<()> {
    let ch = chain_str(&e.chain);
    let sig = if e.signature.is_empty() { e.name.clone() } else { e.signature.clone() };
    let mut ann: Vec<String> = Vec::new();
    let fl = sym_flags(e.flags, e.file_flags);
    if !fl.is_empty() {
        ann.push(format!("[{}]", fl.join(",")));
    }
    if !e.supers.is_empty() {
        ann.push(format!(": {}", e.supers.join(", ")));
    }
    if e.reach >= 0.8 {
        ann.push(format!("reach {:.1}", e.reach));
    }
    let ann = if ann.is_empty() { String::new() } else { format!("   {}", ann.join(" ")) };
    let name = if show_name { format!("{}  ", e.name) } else { String::new() };
    writeln!(w, "  {:<9} {}:{}  {}{}{}{}{}", e.kind.name(), e.rel, e.line, name, ch, if ch.is_empty() { "" } else { " › " }, sig, ann)?;
    if let Some(d) = &e.doc {
        writeln!(w, "            \"{d}\"")?;
    }
    Ok(())
}

pub fn run_def(c: &Common, o: &Options, name: &str, from: &[String], def_kind: Option<&str>) -> Result<()> {
    let want = def_kind.map(parse_def_kind).transpose()?;
    let mut from: Vec<String> = from.to_vec();
    if from.is_empty()
        && !c.no_session
        && let Some(s) = greeg_query::session::Session::open(o, c.session.as_deref())
    {
        from = s.focus();
    }
    let r = verbs::def(o, name, &from, want)?;
    let mut w = out();
    if c.json {
        for e in &r.entries {
            let mut v = def_json(e);
            v["type"] = json!("def");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"def","name":r.name,"shown":r.entries.len(),"total":r.total,"source":r.source,"rung":r.rung.name(),"suggestions":r.suggestions,"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    if r.entries.is_empty() {
        writeln!(w, "def {}  no definition found ({})", r.name, r.source)?;
        if !r.suggestions.is_empty() {
            writeln!(w, "closest names: {}", r.suggestions.join(", "))?;
        } else {
            writeln!(w, "next: greeg {name} --kind def  |  greeg -i {name}")?;
        }
        w.flush()?;
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    let rung = if r.rung != greeg_query::Rung::Exact { format!(" · matched {}", r.rung.describe()) } else { String::new() };
    writeln!(w, "def {}  {} of {} definitions{} · {} · {:.0} ms", r.name, r.entries.len(), r.total, rung, r.source, r.elapsed_ms)?;
    let multi_name = r.entries.iter().any(|e| e.name != r.name);
    for e in &r.entries {
        write_def_entry(&mut w, e, multi_name)?;
    }
    if r.total > r.entries.len() {
        writeln!(w, "  +{} more (raise --budget)", r.total - r.entries.len())?;
    }
    if let Some(top) = r.entries.first() {
        writeln!(w, "next: greeg refs {}  |  greeg callers {}  |  greeg outline {}", r.name, r.name, top.rel)?;
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
    let mut by_kind: Vec<(HitKind, Vec<(usize, usize)>)> = HitKind::ALL.iter().map(|k| (*k, Vec::new())).collect();
    for (fi, f) in s.files.iter().enumerate() {
        for (hi, h) in f.hits.iter().enumerate() {
            by_kind[h.kind.idx()].1.push((fi, hi));
        }
    }
    for (_, v) in by_kind.iter_mut() {
        v.sort_by(|a, b| s.files[b.0].hits[b.1].score.partial_cmp(&s.files[a.0].hits[a.1].score).unwrap_or(std::cmp::Ordering::Equal).then(s.files[a.0].rel.cmp(&s.files[b.0].rel)).then(s.files[a.0].hits[a.1].line.cmp(&s.files[b.0].hits[b.1].line)));
    }
    let total_lines: usize = if o.budget == 0 { usize::MAX } else { (o.budget / 22).max(6) };
    let nonempty: Vec<&(HitKind, Vec<(usize, usize)>)> = by_kind.iter().filter(|(_, v)| !v.is_empty()).collect();
    let weight_sum: f64 = nonempty.iter().map(|(_, v)| (v.len() as f64).sqrt()).sum();
    let resolved = if r.classified > 0 { format!(" · {}% resolved to a definition", r.resolved * 100 / r.classified) } else { String::new() };
    if c.json {
        for e in &r.defs {
            let mut v = def_json(e);
            v["type"] = json!("def");
            serde_json::to_writer(&mut w, &v)?;
            writeln!(w)?;
        }
        for (k, v) in &nonempty {
            let share = if total_lines == usize::MAX { v.len() } else { ((total_lines as f64 * (v.len() as f64).sqrt() / weight_sum.max(1.0)).round() as usize).clamp(2, v.len()) };
            for &(fi, hi) in v.iter().take(share) {
                let f = &s.files[fi];
                let h = &f.hits[hi];
                serde_json::to_writer(&mut w, &json!({"type":"ref","data":{"kind":k.name(),"path":f.rel,"line":h.line,"text":String::from_utf8_lossy(&h.text),"symbol":chain_str(&h.chain),"file_flags":f.flags.names(),"score":h.score}}))?;
                writeln!(w)?;
            }
        }
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"refs","name":name,"hits_total":s.stats.total_hits,"files_total":s.stats.files_matched,"by_kind":nonempty.iter().map(|(k,v)| json!([k.name(), v.len()])).collect::<Vec<_>>(),"resolved":r.resolved,"classified":r.classified,"rung":s.rung.name(),"source":s.stats.source,"elapsed_ms":s.stats.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    if s.stats.total_hits == 0 {
        writeln!(w, "refs {name}  no references ({})", s.stats.source)?;
        w.flush()?;
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    let rung = if s.rung != greeg_query::Rung::Exact { format!(" · matched {}", s.rung.describe()) } else { String::new() };
    writeln!(w, "refs {}  {} hits · {} files{}{} · {} · {:.0} ms", name, fmt_n(s.stats.total_hits), fmt_n(s.stats.files_matched), resolved, rung, s.stats.source, s.stats.elapsed_ms)?;
    if !r.defs.is_empty() {
        let d: Vec<String> = r.defs.iter().take(3).map(|e| format!("{}:{} ({})", e.rel, e.line, e.kind.name())).collect();
        writeln!(w, "defined at  {}", d.join("  "))?;
    }
    let mut shown = 0usize;
    for (k, v) in &nonempty {
        let share = if total_lines == usize::MAX { v.len() } else { ((total_lines as f64 * (v.len() as f64).sqrt() / weight_sum.max(1.0)).round() as usize).clamp(2, v.len()) };
        writeln!(w, "\n{} ({})", k.name(), fmt_n(v.len()))?;
        let mut seen_lines: Vec<(usize, u32)> = Vec::new();
        let mut taken = 0usize;
        for &(fi, hi) in v.iter() {
            if taken >= share {
                break;
            }
            let f = &s.files[fi];
            let h = &f.hits[hi];
            if seen_lines.contains(&(fi, h.line)) {
                continue;
            }
            seen_lines.push((fi, h.line));
            taken += 1;
            let ch = hit_chain(h);
            let flag = if f.flags.demoted() { format!(" [{}]", f.flags.names().join(",")) } else { String::new() };
            writeln!(w, "  {}:{}{}  {}{}{}", f.rel, h.line, flag, ch, if ch.is_empty() { "" } else { " › " }, String::from_utf8_lossy(&h.text).trim())?;
            shown += 1;
        }
        if v.len() > taken {
            writeln!(w, "  +{} more", v.len() - taken)?;
        }
    }
    writeln!(w, "\n{} of {} hits shown · next: greeg callers {name}  |  greeg impact {name}  |  greeg refs {name} --kind call", shown, fmt_n(s.stats.total_hits))?;
    w.flush()?;
    Ok(())
}

pub fn run_callers(c: &Common, o: &Options, name: &str, depth: usize) -> Result<()> {
    let r = verbs::callers(o, name, depth)?;
    let mut w = out();
    let limit = if o.budget == 0 { usize::MAX } else { (o.budget / 30).max(5) };
    if c.json {
        for cl in r.callers.iter().take(limit) {
            serde_json::to_writer(&mut w, &json!({"type":"caller","data":{"path":cl.rel,"symbol":chain_str(&cl.chain),"kind":cl.chain.last().map(|(k,_)| k.name()),"def_line":cl.def_line,"count":cl.count,"lines":cl.lines,"file_flags":cl.file_flags.names(),"called_by":cl.called_by}}))?;
            writeln!(w)?;
        }
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"callers","name":r.name,"callers":r.callers.len(),"call_sites":r.total_hits,"files":r.files,"source":r.source,"rung":r.rung.name(),"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    if r.callers.is_empty() {
        writeln!(w, "callers {}  no call sites ({})", r.name, r.source)?;
        w.flush()?;
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    writeln!(w, "callers {}  {} call sites in {} functions · {} files · {} · {:.0} ms", r.name, fmt_n(r.total_hits), fmt_n(r.callers.len()), fmt_n(r.files), r.source, r.elapsed_ms)?;
    for cl in r.callers.iter().take(limit) {
        let sym = if cl.chain.is_empty() { "(top level)".to_string() } else { chain_str(&cl.chain) };
        let kind = cl.chain.last().map(|(k, _)| k.name()).unwrap_or("");
        let lines: Vec<String> = cl.lines.iter().map(|l| l.to_string()).collect();
        let flag = if cl.file_flags.demoted() { format!(" [{}]", cl.file_flags.names().join(",")) } else { String::new() };
        writeln!(w, "  {:<7} {}:{}{}  {}  ×{}  (lines {})", kind, cl.rel, cl.def_line, flag, sym, cl.count, lines.join(", "))?;
        if !cl.called_by.is_empty() {
            writeln!(w, "          ← called by {}", cl.called_by.join(", "))?;
        }
    }
    if r.callers.len() > limit {
        writeln!(w, "  +{} more callers (raise --budget)", r.callers.len() - limit)?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_impls(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = verbs::impls(o, name)?;
    let mut w = out();
    let limit = if o.budget == 0 { usize::MAX } else { (o.budget / 30).max(5) };
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
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"impls","name":r.name,"direct":r.direct.len(),"extras":r.extras.len(),"source":r.source,"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    if r.direct.is_empty() && r.extras.is_empty() {
        writeln!(w, "impls {}  none found ({})", r.name, r.source)?;
        w.flush()?;
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    writeln!(w, "impls {}  {} implementations{} · {} · {:.0} ms", r.name, r.direct.len(), if r.extras.is_empty() { String::new() } else { format!(" + {} low-confidence", r.extras.len()) }, r.source, r.elapsed_ms)?;
    for e in r.direct.iter().take(limit) {
        write_def_entry(&mut w, e, true)?;
    }
    if r.direct.len() > limit {
        writeln!(w, "  +{} more", r.direct.len() - limit)?;
    }
    if !r.extras.is_empty() {
        writeln!(w, "low confidence ({} in type position on definition lines)", r.name)?;
        for e in r.extras.iter().take(limit / 2 + 1) {
            write_def_entry(&mut w, e, true)?;
        }
    }
    w.flush()?;
    Ok(())
}

pub fn run_outline(c: &Common, o: &Options, file: &str) -> Result<()> {
    let r = verbs::outline(o, file)?;
    let mut w = out();
    if c.json {
        for d in &r.defs {
            serde_json::to_writer(&mut w, &json!({"type":"symbol","data":{"path":r.rel,"name":d.name,"kind":d.kind.name(),"line":d.line,"start":d.start,"end":d.end,"container":chain_str(&d.chain[..d.chain.len().saturating_sub(1)]),"depth":d.chain.len().saturating_sub(1),"flags":sym_flags(d.flags, FileFlags::default())}}))?;
            writeln!(w)?;
        }
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"outline","path":r.rel,"symbols":r.defs.len(),"imports":r.imports,"source":r.source,"parse_errors":r.parse_errors,"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    writeln!(w, "outline {}  {} symbols · {} imports · {} · {} · {:.0} ms", r.rel, r.defs.len(), r.imports.len(), r.lang.name(), r.source, r.elapsed_ms)?;
    if r.parse_errors {
        writeln!(w, "  (file has parse errors; some definitions may come from the regex fallback)")?;
    }
    if !r.imports.is_empty() {
        let imps: Vec<&str> = r.imports.iter().map(|s| s.as_str()).take(12).collect();
        writeln!(w, "  imports  {}{}", imps.join(", "), if r.imports.len() > 12 { format!(" +{}", r.imports.len() - 12) } else { String::new() })?;
    }
    // budget: collapse depth when the tree is large
    let max_lines = if o.budget == 0 { usize::MAX } else { (o.budget / 14).max(10) };
    let mut max_depth = 8usize;
    while max_depth > 0 && r.defs.iter().filter(|d| d.chain.len() <= max_depth + 1).count() > max_lines {
        max_depth -= 1;
    }
    let mut printed = 0usize;
    let mut hidden_under: Option<(usize, usize)> = None; // (index of parent line, hidden count)
    let mut hidden_counts: Vec<usize> = vec![0; r.defs.len()];
    for (i, d) in r.defs.iter().enumerate() {
        let depth = d.chain.len().saturating_sub(1);
        if depth > max_depth {
            // attribute to the nearest visible ancestor
            let mut j = i;
            while j > 0 {
                j -= 1;
                if r.defs[j].chain.len().saturating_sub(1) <= max_depth && d.start >= r.defs[j].start && d.end <= r.defs[j].end {
                    hidden_counts[j] += 1;
                    break;
                }
            }
            continue;
        }
        printed += 1;
    }
    let _ = &mut hidden_under;
    let mut n = 0usize;
    for (i, d) in r.defs.iter().enumerate() {
        let depth = d.chain.len().saturating_sub(1);
        if depth > max_depth {
            continue;
        }
        n += 1;
        if n > max_lines {
            writeln!(w, "  +{} more symbols (raise --budget)", printed - max_lines)?;
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
        let ann = if ann.is_empty() { String::new() } else { format!("  [{}]", ann.join(",")) };
        let more = if hidden_counts[i] > 0 { format!("  (+{} nested)", hidden_counts[i]) } else { String::new() };
        writeln!(w, "  {}{:<8} {}  :{}{}{}", "  ".repeat(depth), d.kind.name(), d.name, d.line, ann, more)?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_map(c: &Common, o: &Options, dir: &str) -> Result<()> {
    let r = verbs::map(o, dir)?;
    let mut w = out();
    let file_limit = if o.budget == 0 { usize::MAX } else { (o.budget / 45).clamp(5, 60) };
    let dir_limit = if o.budget == 0 { usize::MAX } else { (o.budget / 60).clamp(4, 24) };
    if c.json {
        for d in r.dirs.iter().take(dir_limit) {
            serde_json::to_writer(&mut w, &json!({"type":"dir","data":{"path":d.rel,"files":d.files,"symbols":d.symbols,"rank":d.rank}}))?;
            writeln!(w)?;
        }
        for f in r.files.iter().take(file_limit) {
            serde_json::to_writer(&mut w, &json!({"type":"file","data":{"path":f.rel,"rank":f.rank,"symbols":f.symbols,"imported_by":f.imported_by,"by_kind":f.by_kind.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"top":f.top.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names()}}))?;
            writeln!(w)?;
        }
        serde_json::to_writer(&mut w, &json!({"type":"footer","data":{"verb":"map","dir":r.dir,"files_total":r.files_total,"symbols_total":r.symbols_total,"dirs_total":r.dirs.len(),"source":r.source,"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    writeln!(w, "map {}  {} files · {} symbols · {} subdirectories · {} · {:.0} ms", if r.dir.is_empty() { "." } else { &r.dir }, fmt_n(r.files_total), fmt_n(r.symbols_total), r.dirs.len(), r.source, r.elapsed_ms)?;
    if !r.dirs.is_empty() {
        writeln!(w, "\ndirectories (by best file rank)")?;
        for d in r.dirs.iter().take(dir_limit) {
            writeln!(w, "  {:<40} {:>5} files {:>7} symbols  rank {:.2}", format!("{}/", d.rel), d.files, fmt_n(d.symbols), d.rank)?;
        }
        if r.dirs.len() > dir_limit {
            writeln!(w, "  +{} more directories", r.dirs.len() - dir_limit)?;
        }
    }
    writeln!(w, "\nfiles (by import PageRank)")?;
    for f in r.files.iter().take(file_limit) {
        let kinds: Vec<String> = f.by_kind.iter().map(|(k, n)| format!("{} {}", n, k.name())).collect();
        let top: Vec<String> = f.top.iter().map(|(k, n)| format!("{} {}", k.name(), n)).collect();
        let flag = if f.flags.demoted() { format!(" [{}]", f.flags.names().join(",")) } else { String::new() };
        writeln!(w, "  {}{}  rank {:.2} · ←{} · {}", f.rel, flag, f.rank, f.imported_by, kinds.join(", "))?;
        if !top.is_empty() {
            writeln!(w, "      {}", top.join(", "))?;
        }
    }
    if r.files.len() > file_limit {
        writeln!(w, "  +{} more files (raise --budget or narrow the directory)", r.files.len() - file_limit)?;
    }
    if let Some(d) = r.dirs.first() {
        writeln!(w, "next: greeg map {}  |  greeg outline {}", d.rel, r.files.first().map(|f| f.rel.as_str()).unwrap_or(""))?;
    }
    w.flush()?;
    Ok(())
}

pub fn run_impact(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = verbs::impact(o, name)?;
    let mut w = out();
    let per_group = if o.budget == 0 { usize::MAX } else { (o.budget / 90).clamp(3, 30) };
    let write_group = |w: &mut dyn Write, title: &str, why: &str, files: &[verbs::ImpactFile]| -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let hits: usize = files.iter().map(|f| f.hits).sum();
        writeln!(w, "\n{} ({} files, {} hits) — {}", title, files.len(), fmt_n(hits), why)?;
        for f in files.iter().take(per_group) {
            let kinds: Vec<String> = f.kinds.iter().map(|(k, n)| format!("{} {}", k.name(), n)).collect();
            writeln!(w, "  {}  {}", f.rel, kinds.join(", "))?;
            for (l, t) in f.sample.iter().take(2) {
                writeln!(w, "      {:>5}  {}", l, t.trim())?;
            }
        }
        if files.len() > per_group {
            writeln!(w, "  +{} more files", files.len() - per_group)?;
        }
        Ok(())
    };
    if c.json {
        let grp = |files: &[verbs::ImpactFile]| -> Vec<serde_json::Value> { files.iter().map(|f| json!({"path":f.rel,"hits":f.hits,"kinds":f.kinds.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names(),"sample":f.sample})).collect() };
        serde_json::to_writer(&mut w, &json!({"type":"impact","data":{"name":r.name,"definitions":r.defs.iter().map(def_json).collect::<Vec<_>>(),"will_break":grp(&r.will_break),"may_break":grp(&r.may_break),"review":grp(&r.review),
            "callers":r.callers.callers.iter().take(per_group).map(|cl| json!({"path":cl.rel,"symbol":chain_str(&cl.chain),"count":cl.count,"called_by":cl.called_by})).collect::<Vec<_>>(),
            "total_hits":r.total_hits,"elapsed_ms":r.elapsed_ms}}))?;
        writeln!(w)?;
        w.flush()?;
        return Ok(());
    }
    if r.total_hits == 0 {
        writeln!(w, "impact {}  no references found", r.name)?;
        w.flush()?;
        greeg_query::indexed::flush_pending_build();
        std::process::exit(1);
    }
    writeln!(w, "impact {}  {} hits · {} files · {} definitions · {:.0} ms", r.name, fmt_n(r.total_hits), r.will_break.len() + r.may_break.len() + r.review.len(), r.defs.len(), r.elapsed_ms)?;
    for e in r.defs.iter().take(3) {
        write_def_entry(&mut w, e, false)?;
    }
    write_group(&mut w, "WILL BREAK", "calls, type uses or imports in source", &r.will_break)?;
    write_group(&mut w, "MAY BREAK", "member or bare identifier uses", &r.may_break)?;
    write_group(&mut w, "REVIEW", "tests, comments, strings, demoted files", &r.review)?;
    if !r.callers.callers.is_empty() {
        writeln!(w, "\ncallers ({} functions, depth 2)", r.callers.callers.len())?;
        for cl in r.callers.callers.iter().take(per_group) {
            let sym = if cl.chain.is_empty() { "(top level)".to_string() } else { chain_str(&cl.chain) };
            writeln!(w, "  {}:{}  {}  ×{}{}", cl.rel, cl.def_line, sym, cl.count, if cl.called_by.is_empty() { String::new() } else { format!("  ← {}", cl.called_by.join(", ")) })?;
        }
    }
    w.flush()?;
    Ok(())
}
