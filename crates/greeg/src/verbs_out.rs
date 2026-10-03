//! Text and JSON rendering for the symbol verbs (ARCHITECTURE.md).
//! Every text layout groups entries by file: the path once as a header, then
//! `  <line> <kind> <text>` rows beneath it.

use crate::{Common, chain_str, container_of, fmt_n};
use anyhow::Result;
use greeg_lang::sym::{SYM_EXPORTED, SYM_HAS_DOC, SYM_TEST};
use greeg_lang::{DefKind, FileFlags};
use greeg_query::indexed::answered;
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
        "complete":o.complete(),"source":o.source,"fresh":o.fresh,"deferred":o.deferred,
        "truncated_by":o.truncated_by()})
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

/// The outcome line that ends a text answer: what the budget cut
/// (`shown/total unit`), a relaxed match, an index the freshness check
/// skipped, and how to see the rest. A complete, exact, checked answer has none.
pub(crate) fn outcome_line(
    w: &mut impl Write,
    counts: &[(usize, usize, &str)],
    o: &Outcome,
    more: &str,
) -> Result<()> {
    let mut terms: Vec<String> = counts
        .iter()
        .filter(|(shown, total, _)| shown < total)
        .map(|(shown, total, unit)| format!("{}/{} {unit}", fmt_n(*shown), fmt_n(*total)))
        .collect();
    let cut = !terms.is_empty();
    if !o.exact() {
        terms.push(format!("matched {}", o.rung.describe()));
    }
    if let Some(note) = unchecked_note(o) {
        terms.push(note.to_string());
    }
    if cut {
        terms.push(more.to_string());
    }
    if !terms.is_empty() {
        writeln!(w, "{}", terms.join(" · "))?;
    }
    Ok(())
}

/// An index answer under `--fresh none` was not checked against the tree.
pub(crate) fn unchecked_note(o: &Outcome) -> Option<&'static str> {
    (o.source != "scan" && o.fresh == "none").then_some("index not checked for changes")
}

/// How an answer with nothing cut would say how to see more.
const MORE: &str = "raise --budget";

/// A rendering's row allowance, whether `--max-bytes` set it, and whether
/// the budget had cut rows before that.
#[derive(Clone, Copy)]
struct Cut {
    rows: usize,
    bytes: bool,
    budget: bool,
}

impl Cut {
    /// How to see what was left out.
    fn more(self) -> &'static str {
        match (self.bytes, self.budget) {
            (true, true) => "raise --budget and --max-bytes",
            (true, false) => "raise --max-bytes",
            _ => "raise --budget",
        }
    }

    /// `o`, naming the limit that cut it.
    fn mark(self, o: Outcome) -> Outcome {
        Outcome {
            byte_cut: self.bytes,
            ..o
        }
    }
}

/// A fitted answer, the row allowance it was rendered at out of `max`, and
/// the limits that cut it (`budget`, `bytes`).
struct Fitted {
    text: Vec<u8>,
    oc: Outcome,
    rows: usize,
    max: usize,
    cut_by: Vec<&'static str>,
}

/// An answer at a row allowance in `0..=max` whose estimated tokens fit
/// `o.budget` (0: unlimited) and, under `--max-bytes`, whose bytes fit too.
/// The search assumes an answer grows with its allowance, so where a completed
/// group drops its `+N more` line a slightly larger allowance can be missed.
/// Allowance 0 is the answer's floor (header, counts, outcome): written even
/// over the budget, but an error over `--max-bytes`.
fn fit(
    json: bool,
    o: &Options,
    max: usize,
    render: impl Fn(Cut) -> Result<(Vec<u8>, Outcome)>,
) -> Result<Fitted> {
    // JSON is measured with timings at a fixed width, so a run's clock cannot move the fit
    fn measured(json: bool, b: &[u8]) -> std::borrow::Cow<'_, [u8]> {
        if json {
            greeg_query::tokens::timeless(b)
        } else {
            b.into()
        }
    }
    let tokens_fit =
        |b: &[u8]| o.budget == 0 || greeg_query::tokens::estimate(&measured(json, b)) <= o.budget;
    let bytes_fit = |b: &[u8]| o.max_bytes == 0 || measured(json, b).len() <= o.max_bytes;
    // the largest allowance in `0..=hi` whose rendering passes, if any
    type Rendered = (usize, (Vec<u8>, Outcome));
    let largest = |hi: usize,
                   (bytes, budget): (bool, bool),
                   pass: &dyn Fn(&[u8]) -> bool|
     -> Result<Option<Rendered>> {
        let top = render(Cut {
            rows: hi,
            bytes,
            budget,
        })?;
        if pass(&top.0) {
            return Ok(Some((hi, top)));
        }
        let (mut lo, mut hi, mut best) = (0, hi, None);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = render(Cut {
                rows: mid,
                bytes,
                budget,
            })?;
            if pass(&r.0) {
                lo = mid + 1;
                best = Some((mid, r));
            } else {
                hi = mid;
            }
        }
        Ok(best)
    };
    let (rows, by_budget) = match largest(max, (false, false), &tokens_fit)? {
        Some(f) => f,
        None => (
            0,
            render(Cut {
                rows: 0,
                bytes: false,
                budget: false,
            })?,
        ),
    };
    if bytes_fit(&by_budget.0) {
        let cut_by = if rows < max { vec!["budget"] } else { vec![] };
        let (text, oc) = by_budget;
        return Ok(Fitted {
            text,
            oc,
            rows,
            max,
            cut_by,
        });
    }
    // the ceiling cuts further; below the budget's floor only bytes count
    let both = |b: &[u8]| tokens_fit(b) && bytes_fit(b);
    let cut = (true, rows < max);
    let found = match largest(rows, cut, &both)? {
        Some(f) => Some(f),
        None => largest(rows, cut, &bytes_fit)?,
    };
    match found {
        Some((kept, (text, oc))) => Ok(Fitted {
            text,
            oc,
            rows: kept,
            max,
            cut_by: if cut.1 {
                vec!["budget", "bytes"]
            } else {
                vec!["bytes"]
            },
        }),
        None => {
            let floor = render(Cut {
                rows: 0,
                bytes: true,
                budget: rows < max,
            })?;
            anyhow::bail!(
                "--max-bytes {} is below the {} bytes this answer needs",
                o.max_bytes,
                measured(json, &floor.0).len()
            )
        }
    }
}

/// Write a fitted answer and end the run.
fn finish_fit(
    c: &Common,
    o: &Options,
    mut w: impl Write,
    verb: &'static str,
    ex: Option<&crate::explain::VerbExplain>,
    fitted: Fitted,
) -> Result<()> {
    write_fitted(c, o, &mut w, verb, ex, &fitted)?;
    finish(w, verb, &fitted.oc)
}

/// Write a fitted answer and, when asked, its `explain` record: in JSON
/// before the footer, the last record (on stderr under a byte ceiling); in
/// text on stderr. It is outside the budget, the ceiling and statistics.
fn write_fitted(
    c: &Common,
    o: &Options,
    w: &mut impl Write,
    verb: &'static str,
    ex: Option<&crate::explain::VerbExplain>,
    f: &Fitted,
) -> Result<()> {
    let text = &f.text;
    let Some(ex) = ex else {
        w.write_all(text)?;
        return Ok(());
    };
    let fit = if f.cut_by.is_empty() {
        serde_json::Value::Null
    } else {
        json!({"rows": f.rows, "of": f.max, "cut_by": f.cut_by})
    };
    let record = crate::explain::verb(verb, &f.oc, ex, fit);
    if c.json() && o.max_bytes == 0 {
        let at = text[..text.len().saturating_sub(1)]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        w.write_all(&text[..at])?;
        w.flush()?;
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        greeg_index::commit::commit();
        let mut raw = std::io::stdout().lock();
        raw.write_all(&line)?;
        raw.flush()?;
        w.write_all(&text[at..])?;
    } else {
        w.write_all(text)?;
        w.flush()?;
        let mut block = Vec::new();
        if c.json() {
            serde_json::to_writer(&mut block, &record)?;
            block.push(b'\n');
        } else {
            crate::explain::write_verb_text(&mut block, &record, ex)?;
        }
        std::io::stderr().write_all(&block)?;
    }
    Ok(())
}

/// The outcome of `show`, `outline` and `map` text: answered, exact.
fn answered_outcome(
    total: usize,
    shown: usize,
    source: &'static str,
    fresh: &'static str,
) -> Outcome {
    Outcome {
        total,
        shown,
        rung: greeg_query::Rung::Exact,
        source,
        fresh,
        deferred: 0,
        byte_cut: false,
    }
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

pub(crate) fn sym_flags(flags: u8, file_flags: FileFlags) -> Vec<&'static str> {
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

/// A path as verb output shows it (`greeg_index::rel::display`).
fn path_text(rel: &[u8]) -> std::borrow::Cow<'_, str> {
    greeg_index::rel::display(rel)
}

fn file_flag_suffix(rel: &[u8], flags: FileFlags) -> String {
    let mut names = flags.names();
    names.retain(|x| *x != "huge");
    if names.is_empty() && greeg_query::is_mock_path(&path_text(rel)) {
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
        "path":crate::json_rel(&e.rel), "line": e.line, "kind": e.kind.name(), "name": e.name, "container": chain_str(&e.chain),
        "signature": e.signature, "doc": e.doc, "flags": sym_flags(e.flags, e.file_flags), "supertypes": e.supers,
        "score": e.score, "reach": e.reach, "start": e.start, "end": e.end
    })
}

/// The first `n` items, or all of them.
fn head<T>(v: &[T], n: usize) -> &[T] {
    &v[..v.len().min(n)]
}

fn digits(n: u32) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// Definition entries grouped by file in order of first appearance:
/// `  <line> <kind>  [name  ]container › signature   [flags] : supers [reach]`.
/// A body beneath its row, dedented by its first line's indentation as the
/// block layout prints it (`  NNN  text`), then what was cut.
fn write_body(w: &mut impl Write, b: &verbs::Body, lw: usize, more: &str) -> Result<()> {
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
            "  … {} more lines to {} ({more})",
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
    bodies: Option<(&[Option<verbs::Body>], &str)>,
) -> Result<()> {
    let mut order: Vec<&[u8]> = Vec::new();
    for e in entries {
        if !order.contains(&e.rel.as_slice()) {
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
        writeln!(
            w,
            "{}{}",
            path_text(rel),
            file_flag_suffix(rel, group[0].1.file_flags)
        )?;
        let lw = group
            .iter()
            .map(|(i, e)| {
                let body_last = bodies
                    .and_then(|(bs, _)| bs[*i].as_ref())
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
            if let Some((b, more)) = bodies.and_then(|(bs, more)| Some((bs[i].as_ref()?, more))) {
                write_body(w, b, lw, more)?;
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
    let r = answered(o, |o| verbs::def(o, name, &from, want))?;
    let ex = c.explain.then(|| crate::explain::VerbExplain {
        ranked: &r.entries,
        ..crate::explain::VerbExplain::new(json!({"definitions": r.total, "described": r.entries.len(), "near_names": r.suggestions.len()}))
    });
    let oc = Outcome {
        total: r.total,
        shown: r.entries.len(),
        rung: r.rung.clone(),
        source: r.source,
        fresh: r.fresh,
        deferred: 0,
        byte_cut: false,
    };
    let w = out();
    if c.json() {
        // the allowance is entries, or near names when there is no entry
        let near = r.entries.is_empty();
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let (shown, suggestions) = if near {
                (0, r.suggestions.len().min(limit))
            } else {
                (r.entries.len().min(limit), r.suggestions.len())
            };
            let oc = cut.mark(Outcome {
                shown,
                ..oc.clone()
            });
            if c.json_greeg() {
                crate::json_native::defs(&mut w, &r, shown, suggestions, &oc)?;
                return Ok((w, oc));
            }
            for e in &r.entries[..shown] {
                let mut v = def_json(e);
                v["type"] = json!("def");
                serde_json::to_writer(&mut w, &v)?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"def","name":r.name,"shown":shown,"total":r.total,"source":r.source,"rung":r.rung.name(),"suggestions":r.suggestions[..suggestions],"suggestions_total":r.suggestions.len(),"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let max = if near {
            r.suggestions.len()
        } else {
            r.entries.len()
        };
        let fitted = fit(c.json(), o, max, render)?;
        return finish_fit(c, o, w, "def", ex.as_ref(), fitted);
    }
    if r.entries.is_empty() {
        let names = &r.suggestions;
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            writeln!(w, "def {}  no definition found ({})", r.name, r.source)?;
            if names.is_empty() {
                writeln!(w, "next: greeg {name} --kind def | greeg -i {name}")?;
            } else {
                let shown = &names[..names.len().min(limit)];
                let left = names.len() - shown.len();
                writeln!(
                    w,
                    "closest names: {}{}",
                    shown.join(", "),
                    match (shown.is_empty(), left) {
                        (_, 0) => String::new(),
                        (true, n) => format!("{n} ({})", cut.more()),
                        (false, n) => format!(" +{n} ({})", cut.more()),
                    }
                )?;
            }
            let oc = cut.mark(oc.clone());
            outcome_line(&mut w, &[], &oc, cut.more())?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, names.len(), render)?;
        return finish_fit(c, o, w, "def", ex.as_ref(), fitted);
    }
    let multi_name = r.entries.iter().any(|e| e.name != r.name);
    let entries: Vec<&DefEntry> = r.entries.iter().collect();
    // `--mode block`: each definition's body follows its row, the budget
    // shared in order (a module file has no body of its own)
    let bodies: Option<Vec<Option<verbs::Body>>> = if o.mode == greeg_query::Mode::Block {
        let mut est = 0usize;
        let mut sources: std::collections::HashMap<&[u8], greeg_query::Source> = Default::default();
        let mut v = Vec::with_capacity(entries.len());
        for e in &entries {
            if e.file_module {
                v.push(None);
                continue;
            }
            if !sources.contains_key(e.rel.as_slice())
                && let Ok(src) = verbs::source_of(o, &e.rel)
            {
                sources.insert(e.rel.as_slice(), src);
            }
            v.push(sources.get(e.rel.as_slice()).map(|src| {
                let end = src.end_line(e.start, e.end);
                verbs::body(o, src, e.line, end, &mut est)
            }));
        }
        Some(v)
    } else {
        None
    };
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let limit = cut.rows;
        let mut w = Vec::new();
        let shown = entries.len().min(limit);
        let oc = cut.mark(Outcome {
            shown,
            ..oc.clone()
        });
        writeln!(
            w,
            "def {}  {} of {} definitions{} · {}{}",
            r.name,
            shown,
            r.total,
            relaxed_note(&r.rung),
            r.source,
            ms(c, r.elapsed_ms)
        )?;
        write_def_groups(
            &mut w,
            &entries[..shown],
            multi_name,
            explicit_from,
            c.chain,
            bodies.as_deref().map(|b| (&b[..shown], cut.more())),
        )?;
        if r.total > shown {
            writeln!(w, "  +{} more", r.total - shown)?;
        }
        outcome_line(&mut w, &[(shown, r.total, "definitions")], &oc, cut.more())?;
        if let Some(top) = r.entries.first() {
            writeln!(
                w,
                "next: refs {} | callers {} | outline {}",
                r.name,
                r.name,
                path_text(&top.rel)
            )?;
        }
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, entries.len(), render)?;
    finish_fit(c, o, w, "def", ex.as_ref(), fitted)
}

pub fn run_show(c: &Common, o: &Options, locs: &[(String, u32)]) -> Result<()> {
    let r = answered(o, |o| verbs::show(o, locs))?;
    let ex = c.explain.then(|| {
        let found = r.items.iter().filter(|i| i.def.is_some()).count();
        crate::explain::VerbExplain::new(json!({"locations": locs.len(), "in_a_definition": found}))
    });
    let mut w = out();
    // the allowance is body lines, shared in order
    let bodies = |limit: usize| -> Vec<verbs::Body> {
        let mut left = limit;
        r.items
            .iter()
            .map(|it| {
                let keep = it.body.lines.len().min(left);
                left -= keep;
                verbs::Body {
                    first: it.body.first,
                    lines: it.body.lines[..keep].to_vec(),
                    last: it.body.last,
                    clipped: it.body.clipped || keep < it.body.lines.len(),
                }
            })
            .collect()
    };
    let max = r.items.iter().map(|it| it.body.lines.len()).sum();
    let n = r.items.len();
    let oc = answered_outcome(n, n, r.source, r.fresh);
    if c.json() {
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let bodies = bodies(limit);
            let oc = cut.mark(oc.clone());
            if c.json_greeg() {
                crate::json_native::show(&mut w, &r, &bodies, &oc)?;
                return Ok((w, oc));
            }
            for (it, body) in r.items.iter().zip(&bodies) {
                let shown_to = body.first + body.lines.len().saturating_sub(1) as u32;
                let text = crate::json_rel(&body.lines.join(&b'\n'));
                let symbol = it.def.as_ref().map(|d| {
                    json!({"name":d.name,"kind":d.kind.name(),"container":chain_str(&d.chain[..d.chain.len().saturating_sub(1)])})
                });
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"show","data":{"path":crate::json_rel(&it.rel),"line":it.asked,"symbol":symbol,"start_line":body.first,"end_line":body.last,"shown_to":shown_to,"clipped":body.clipped,"text":text}}),
                )?;
                writeln!(w)?;
            }
            let complete = !bodies.iter().any(|b| b.clipped);
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"show","items":n,"source":r.source,"elapsed_ms":r.elapsed_ms,
                    "outcome":crate::json_native::OutcomeRec::answered(&oc, complete)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, max, render)?;
        write_fitted(c, o, &mut w, "show", ex.as_ref(), &fitted)?;
        w.flush()?;
        return Ok(());
    }
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let limit = cut.rows;
        let mut w = Vec::new();
        for ((n, it), body) in r.items.iter().enumerate().zip(bodies(limit)) {
            let what = match &it.def {
                Some(d) => format!("{} {}", d.kind.name(), chain_str(&d.chain)),
                None => "no enclosing definition".to_string(),
            };
            writeln!(
                w,
                "show {}:{}  {} · lines {}–{} · {}{}",
                path_text(&it.rel),
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
            write_body(&mut w, &body, lw, cut.more())?;
        }
        let oc = cut.mark(oc.clone());
        outcome_line(&mut w, &[], &oc, cut.more())?;
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, max, render)?;
    write_fitted(c, o, &mut w, "show", ex.as_ref(), &fitted)?;
    w.flush()?;
    Ok(())
}

pub fn run_refs(c: &Common, o: &Options, name: &str) -> Result<()> {
    let kinds: Vec<HitKind> = o.kinds.clone();
    let r = answered(o, |o| verbs::refs(o, name, &kinds))?;
    let ex = c.explain.then(|| {
        let st = &r.scan.stats;
        crate::explain::VerbExplain::new(json!({"definitions": r.defs_total, "files_searched": st.files_searched, "files_matched": st.files_matched, "hits": st.total_hits, "classified": r.classified, "resolved": r.resolved}))
    });
    let s = &r.scan;
    let w = out();
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
    let collapse_imports = !c.json() && o.kinds.is_empty();
    let nonempty: Vec<&(HitKind, Vec<(usize, usize)>)> = by_kind
        .iter()
        .filter(|(k, v)| !(v.is_empty() || collapse_imports && *k == HitKind::Import))
        .collect();
    let weight_sum: f64 = nonempty.iter().map(|(_, v)| (v.len() as f64).sqrt()).sum();
    let share_in = |lines: usize, n: usize| -> usize {
        if lines == usize::MAX {
            n
        } else {
            ((lines as f64 * (n as f64).sqrt() / weight_sum.max(1.0)).round() as usize)
                .clamp(n.min(2).min(lines), n)
        }
    };
    let resolved = if let Some(pct) = (r.resolved * 100).checked_div(r.classified) {
        format!(" · {pct}% resolved")
    } else {
        String::new()
    };
    if c.json() {
        // up to three definitions in the floor, more with the allowance
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let lines = cut.rows;
            let mut w = Vec::new();
            let defs = r.defs.len().min(lines.max(3));
            let shown: Vec<(HitKind, usize, usize)> = nonempty
                .iter()
                .flat_map(|(k, v)| {
                    v.iter()
                        .take(share_in(lines, v.len()))
                        .map(|&(fi, hi)| (*k, fi, hi))
                })
                .collect();
            let oc = cut.mark(Outcome::of_search(s, shown.len()));
            if c.json_greeg() {
                let counts = nonempty.iter().map(|(k, v)| (k.name(), v.len())).collect();
                crate::json_native::refs(&mut w, name, &r, defs, &shown, counts, &oc)?;
                return Ok((w, oc));
            }
            for e in &r.defs[..defs] {
                let mut v = def_json(e);
                v["type"] = json!("def");
                serde_json::to_writer(&mut w, &v)?;
                writeln!(w)?;
            }
            for &(k, fi, hi) in &shown {
                let f = &s.files[fi];
                let h = &f.hits[hi];
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"ref","data":{"kind":k.name(),"path":crate::json_rel(&f.rel),"line":h.line,"text":crate::json_rel(&h.display(s.opts.max_columns).0),"symbol":chain_str(&h.chain),"file_flags":f.flags.names(),"score":h.score}}),
                )?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"refs","name":name,"definitions_total":r.defs_total,"hits_total":s.stats.total_hits,"files_total":s.stats.files_matched,"by_kind":nonempty.iter().map(|(k,v)| json!([k.name(), v.len()])).collect::<Vec<_>>(),"resolved":r.resolved,"classified":r.classified,"rung":s.rung.name(),"source":s.stats.source,"elapsed_ms":s.stats.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, total_lines, render)?;
        return finish_fit(c, o, w, "refs", ex.as_ref(), fitted);
    }
    if s.stats.total_hits == 0 {
        let oc = Outcome::of_search(s, 0);
        let render = |_: Cut| -> Result<(Vec<u8>, Outcome)> {
            let mut w = Vec::new();
            writeln!(w, "refs {name}  no references ({})", s.stats.source)?;
            outcome_line(&mut w, &[], &oc, MORE)?;
            Ok((w, oc.clone()))
        };
        let fitted = fit(c.json(), o, 0, render)?;
        return finish_fit(c, o, w, "refs", ex.as_ref(), fitted);
    }
    let fmt = crate::Fmt {
        chain: c.chain,
        stats: c.stats,
        line_numbers: false,
        stdin: false,
        cols: s.opts.max_columns,
    };
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let lines = cut.rows;
        let mut w = Vec::new();
        writeln!(
            w,
            "refs {}  {} hits · {} files{}{} · {}{}",
            name,
            fmt_n(s.stats.total_hits),
            fmt_n(s.stats.files_matched),
            resolved,
            relaxed_note(&s.rung),
            s.stats.source,
            ms(c, s.stats.elapsed_ms)
        )?;
        if !r.defs.is_empty() {
            let d: Vec<String> = r
                .defs
                .iter()
                .take(3)
                .map(|e| format!("{}:{} ({})", path_text(&e.rel), e.line, e.kind.name()))
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
            let rels: Vec<_> = files
                .iter()
                .take(6)
                .map(|&fi| s.files[fi].rel_text())
                .collect();
            let names = crate::short_names(rels.iter().map(|r| &**r));
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
        // the `imported by` line accounts for every import hit
        let mut shown = if collapse_imports {
            by_kind[HitKind::Import.idx()].1.len()
        } else {
            0
        };
        for (k, v) in &nonempty {
            let share = share_in(lines, v.len());
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
        let oc = cut.mark(Outcome::of_search(s, shown));
        writeln!(w)?;
        outcome_line(
            &mut w,
            &[(shown, s.stats.total_hits, "hits")],
            &oc,
            cut.more(),
        )?;
        writeln!(
            w,
            "next: callers {name} | impact {name} | refs {name} --kind call"
        )?;
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, total_lines, render)?;
    finish_fit(c, o, w, "refs", ex.as_ref(), fitted)
}

pub fn run_callers(c: &Common, o: &Options, name: &str, depth: usize) -> Result<()> {
    let r = answered(o, |o| verbs::callers(o, name, depth))?;
    let ex = c.explain.then(|| {
        crate::explain::VerbExplain::new(json!({"depth": depth, "files": r.files, "hits": r.total_hits, "callers": r.callers.len()}))
    });
    let w = out();
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
    if c.json() {
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let oc = cut.mark(Outcome {
                shown: r.callers.len().min(limit),
                ..oc.clone()
            });
            if c.json_greeg() {
                crate::json_native::callers(&mut w, &r, limit, &oc)?;
                return Ok((w, oc));
            }
            for cl in r.callers.iter().take(limit) {
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"caller","data":{"path":crate::json_rel(&cl.rel),"symbol":chain_str(&cl.chain),"kind":cl.chain.last().map(|(k,_)| k.name()),"def_line":cl.def_line,"count":cl.count,"lines":cl.lines,"file_flags":cl.file_flags.names(),"called_by":cl.called_by}}),
                )?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"callers","name":r.name,"callers":r.callers.len(),"call_sites":r.total_hits,"files":r.files,"source":r.source,"rung":r.rung.name(),"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, limit, render)?;
        return finish_fit(c, o, w, "callers", ex.as_ref(), fitted);
    }
    if r.callers.is_empty() {
        let render = |_: Cut| -> Result<(Vec<u8>, Outcome)> {
            let mut w = Vec::new();
            writeln!(w, "callers {}  no call sites ({})", r.name, r.source)?;
            outcome_line(&mut w, &[], &oc, MORE)?;
            Ok((w, oc.clone()))
        };
        let fitted = fit(c.json(), o, 0, render)?;
        return finish_fit(c, o, w, "callers", ex.as_ref(), fitted);
    }
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let limit = cut.rows;
        let mut w = Vec::new();
        let callers: Vec<&verbs::Caller> = r.callers.iter().take(limit).collect();
        let oc = cut.mark(Outcome {
            shown: callers.len(),
            ..oc.clone()
        });
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
        let mut order: Vec<&[u8]> = Vec::new();
        for cl in &callers {
            if !order.contains(&cl.rel.as_slice()) {
                order.push(&cl.rel);
            }
        }
        for rel in order {
            let group: Vec<&verbs::Caller> =
                callers.iter().filter(|cl| cl.rel == rel).copied().collect();
            writeln!(
                w,
                "{}{}",
                path_text(rel),
                file_flag_suffix(rel, group[0].file_flags)
            )?;
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
        if r.callers.len() > callers.len() {
            writeln!(w, "  +{} more", r.callers.len() - callers.len())?;
        }
        outcome_line(&mut w, &[(oc.shown, oc.total, "callers")], &oc, cut.more())?;
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, limit, render)?;
    finish_fit(c, o, w, "callers", ex.as_ref(), fitted)
}

pub fn run_impls(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = answered(o, |o| verbs::impls(o, name))?;
    let ex = c.explain.then(|| {
        crate::explain::VerbExplain::new(
            json!({"direct": r.direct_total, "extras": r.extras_total}),
        )
    });
    let w = out();
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
                .min(if c.json() { limit } else { limit / 2 + 1 }),
        rung: greeg_query::Rung::Exact,
        source: r.source,
        fresh: r.fresh,
        deferred: 0,
        byte_cut: false,
    };
    if c.json() {
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let (direct, extras) = (r.direct.len().min(limit), r.extras.len().min(limit));
            let oc = cut.mark(Outcome {
                shown: direct + extras,
                ..oc.clone()
            });
            if c.json_greeg() {
                crate::json_native::impls(&mut w, &r, direct, extras, &oc)?;
                return Ok((w, oc));
            }
            for (e, confidence) in r.direct[..direct]
                .iter()
                .map(|e| (e, "high"))
                .chain(r.extras[..extras].iter().map(|e| (e, "low")))
            {
                let mut v = def_json(e);
                v["type"] = json!("impl");
                v["confidence"] = json!(confidence);
                serde_json::to_writer(&mut w, &v)?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"impls","name":r.name,"direct":r.direct_total,"extras":r.extras_total,"source":r.source,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, limit, render)?;
        return finish_fit(c, o, w, "impls", ex.as_ref(), fitted);
    }
    if r.direct.is_empty() && r.extras.is_empty() {
        let render = |_: Cut| -> Result<(Vec<u8>, Outcome)> {
            let mut w = Vec::new();
            writeln!(w, "impls {}  none found ({})", r.name, r.source)?;
            outcome_line(&mut w, &[], &oc, MORE)?;
            Ok((w, oc.clone()))
        };
        let fitted = fit(c.json(), o, 0, render)?;
        return finish_fit(c, o, w, "impls", ex.as_ref(), fitted);
    }
    // low-confidence extras get half the allowance
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let limit = cut.rows;
        let mut w = Vec::new();
        let direct: Vec<&DefEntry> = r.direct.iter().take(limit).collect();
        let extras: Vec<&DefEntry> = r
            .extras
            .iter()
            .take(if limit == 0 { 0 } else { limit / 2 + 1 })
            .collect();
        let oc = cut.mark(Outcome {
            shown: direct.len() + extras.len(),
            ..oc.clone()
        });
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
            write_def_groups(&mut w, &extras, true, false, c.chain, None)?;
            if r.extras_total > extras.len() {
                writeln!(w, "  +{} more", r.extras_total - extras.len())?;
            }
        }
        outcome_line(
            &mut w,
            &[(oc.shown, oc.total, "implementations")],
            &oc,
            cut.more(),
        )?;
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, limit, render)?;
    finish_fit(c, o, w, "impls", ex.as_ref(), fitted)
}

pub fn run_outline(c: &Common, o: &Options, file: &str, imports: bool) -> Result<()> {
    let r = answered(o, |o| verbs::outline(o, file))?;
    let ex = c.explain.then(|| {
        crate::explain::VerbExplain::new(json!({"symbols": r.defs.len(), "imports": r.imports.len(), "parse_errors": r.parse_errors}))
    });
    let mut w = out();
    if c.json() {
        // the allowance is symbols, in file order
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let shown = r.defs.len().min(limit);
            let oc = cut.mark(answered_outcome(r.defs.len(), shown, r.source, r.fresh));
            if c.json_greeg() {
                crate::json_native::outline(&mut w, &r, shown, &oc)?;
                return Ok((w, oc));
            }
            for d in &r.defs[..shown] {
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"symbol","data":{"path":crate::json_rel(&r.rel),"name":d.name,"kind":d.kind.name(),"line":d.line,"start":d.start,"end":d.end,"container":chain_str(&d.chain[..d.chain.len().saturating_sub(1)]),"depth":d.chain.len().saturating_sub(1),"flags":sym_flags(d.flags, FileFlags::default())}}),
                )?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"outline","path":crate::json_rel(&r.rel),"symbols":r.defs.len(),"imports":r.imports,"source":r.source,"parse_errors":r.parse_errors,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, r.defs.len(), render)?;
        write_fitted(c, o, &mut w, "outline", ex.as_ref(), &fitted)?;
        w.flush()?;
        return Ok(());
    }
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let max_lines = cut.rows;
        let mut w = Vec::new();
        writeln!(
            w,
            "outline {}  {} symbols · {} imports · {} · {}{}",
            path_text(&r.rel),
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
        // collapse depth when the tree is large
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
                writeln!(w, "  +{} more", printed - max_lines)?;
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
        // nested symbols folded into their parent's line count as cut
        let listed = printed.min(max_lines);
        let oc = cut.mark(answered_outcome(r.defs.len(), listed, r.source, r.fresh));
        outcome_line(
            &mut w,
            &[(listed, r.defs.len(), "symbols")],
            &oc,
            cut.more(),
        )?;
        Ok((w, oc))
    };
    let max_lines = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 12).max(10)
    };
    let fitted = fit(c.json(), o, max_lines, render)?;
    write_fitted(c, o, &mut w, "outline", ex.as_ref(), &fitted)?;
    w.flush()?;
    Ok(())
}

pub fn run_map(c: &Common, o: &Options, dir: &str) -> Result<()> {
    let r = match verbs::map(o, dir) {
        Ok(r) => r,
        Err(e) => {
            if c.json_greeg()
                && let Some(rb) = e.downcast_ref::<greeg_query::indexed::Rebuilding>()
            {
                let mut w = out();
                crate::json_native::map_rebuilding(&mut w, rb.reason, rb.estimate_ms)?;
                w.flush()?;
            } else if c.json()
                && let Some(rb) = e.downcast_ref::<greeg_query::indexed::Rebuilding>()
            {
                let mut w = out();
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"footer","data":{"verb":"map","outcome":{"exit":2,"rebuilding":{"reason":rb.reason,"estimate_ms":rb.estimate_ms}}}}),
                )?;
                writeln!(w)?;
                w.flush()?;
            }
            return Err(e);
        }
    };
    let ex = c.explain.then(|| {
        crate::explain::VerbExplain::new(json!({"files": r.files_total, "symbols": r.symbols_total, "graph_changes": r.graph_changes}))
    });
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
    if c.json() {
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let limit = cut.rows;
            let mut w = Vec::new();
            let (dir_limit, file_limit) = (dir_limit.min(limit), file_limit.min(limit));
            let (dirs, files) = (r.dirs.len().min(dir_limit), r.files.len().min(file_limit));
            let oc = cut.mark(answered_outcome(
                r.dirs.len() + r.files.len(),
                dirs + files,
                r.source,
                r.fresh,
            ));
            if c.json_greeg() {
                crate::json_native::map(&mut w, &r, dir_limit, file_limit, &oc)?;
                return Ok((w, oc));
            }
            for d in &r.dirs[..dirs] {
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"dir","data":{"path":crate::json_rel(&d.rel),"files":d.files,"symbols":d.symbols,"rank":d.rank}}),
                )?;
                writeln!(w)?;
            }
            for f in &r.files[..files] {
                serde_json::to_writer(
                    &mut w,
                    &json!({"type":"file","data":{"path":crate::json_rel(&f.rel),"rank":f.rank,"symbols":f.symbols,"imported_by":f.imported_by,"by_kind":f.by_kind.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"top":f.top.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names()}}),
                )?;
                writeln!(w)?;
            }
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"map","dir":r.dir,"files_total":r.files_total,"symbols_total":r.symbols_total,"dirs_total":r.dirs.len(),"source":r.source,"graph_changes":r.graph_changes,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let fitted = fit(c.json(), o, dir_limit.max(file_limit), render)?;
        write_fitted(c, o, &mut w, "map", ex.as_ref(), &fitted)?;
        w.flush()?;
        return Ok(());
    }
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let limit = cut.rows;
        let mut w = Vec::new();
        let (dir_limit, file_limit) = (dir_limit.min(limit), file_limit.min(limit));
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
        if r.graph_changes > 0 {
            writeln!(
                w,
                "{} added or removed since the graph was built: imports of unchanged files may miss them (`greeg index` rebuilds it)",
                if r.graph_changes == 1 {
                    "1 Kotlin file".to_string()
                } else {
                    format!("{} Kotlin files", fmt_n(r.graph_changes as usize))
                }
            )?;
        }
        if !r.dirs.is_empty() {
            writeln!(w, "\ndirectories (by best file rank)")?;
            for d in r.dirs.iter().take(dir_limit) {
                writeln!(
                    w,
                    "  {:<40} {:>5} files {:>7} symbols  rank {:.2}",
                    format!("{}/", path_text(&d.rel)),
                    d.files,
                    fmt_n(d.symbols),
                    d.rank
                )?;
            }
            if r.dirs.len() > dir_limit {
                writeln!(w, "  +{} more", r.dirs.len() - dir_limit)?;
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
                path_text(&f.rel),
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
            writeln!(w, "  +{} more", r.files.len() - file_limit)?;
        }
        let dirs_shown = r.dirs.len().min(dir_limit);
        let files_shown = r.files.len().min(file_limit);
        let oc = cut.mark(answered_outcome(
            r.dirs.len() + r.files.len(),
            dirs_shown + files_shown,
            r.source,
            r.fresh,
        ));
        outcome_line(
            &mut w,
            &[
                (dirs_shown, r.dirs.len(), "directories"),
                (files_shown, r.files.len(), "files"),
            ],
            &oc,
            &format!("{} or narrow the directory", cut.more()),
        )?;
        if let Some(d) = r.dirs.first() {
            writeln!(
                w,
                "next: map {} | outline {}",
                path_text(&d.rel),
                r.files
                    .first()
                    .map(|f| path_text(&f.rel))
                    .unwrap_or_default()
            )?;
        }
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, dir_limit.max(file_limit), render)?;
    write_fitted(c, o, &mut w, "map", ex.as_ref(), &fitted)?;
    w.flush()?;
    Ok(())
}

pub fn run_impact(c: &Common, o: &Options, name: &str) -> Result<()> {
    let r = answered(o, |o| verbs::impact(o, name))?;
    let ex = c.explain.then(|| {
        crate::explain::VerbExplain::new(json!({"definitions": r.defs_total, "likely": r.likely.len(), "possible": r.possible.len(), "review": r.review.len(), "import_graph": r.import_graph, "callers": r.callers.callers.len(), "hits": r.total_hits}))
    });
    let groups = [&r.likely, &r.possible, &r.review];
    let files = groups.iter().map(|g| g.len()).sum::<usize>();
    let callers_total = r.callers.callers.len();
    let w = out();
    let per_group = if o.budget == 0 {
        usize::MAX
    } else {
        (o.budget / 130).clamp(3, 30)
    };
    let callers_shown = callers_total.min(per_group);
    // an answer counts referring files and callers; JSON lists every file
    let outcome = |files_shown: usize| Outcome {
        total: files + callers_total,
        shown: files_shown + callers_shown,
        ..r.outcome.clone()
    };
    let write_group = |w: &mut dyn Write,
                       title: &str,
                       why: &str,
                       files: &[verbs::ImpactFile],
                       cap: usize|
     -> Result<()> {
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
        for f in files.iter().take(cap) {
            let kinds: Vec<String> = f
                .kinds
                .iter()
                .map(|(k, n)| format!("{} {}", k.name(), n))
                .collect();
            writeln!(
                w,
                "{}{}  {}",
                path_text(&f.rel),
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
                writeln!(w, "  {:>lw$}  {}", l, String::from_utf8_lossy(t).trim())?;
            }
        }
        if files.len() > cap {
            writeln!(w, "  +{} more", files.len() - cap)?;
        }
        Ok(())
    };
    // likely and possible share one allowance of rows; possible keeps a few
    let caps = |per_group: usize| {
        [
            per_group,
            per_group
                .saturating_sub(r.likely.len().min(per_group))
                .max(per_group.min(3)),
            per_group,
        ]
    };
    if c.json() {
        // files and definitions take the allowance; callers keep their old cap
        let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
            let allowance = cut.rows;
            let mut w = Vec::new();
            let caps = caps(allowance);
            let groups = [
                head(&r.likely, caps[0]),
                head(&r.possible, caps[1]),
                head(&r.review, caps[2]),
            ];
            let defs = r.defs.len().min(allowance);
            let callers = callers_shown.min(allowance);
            let oc = cut.mark(Outcome {
                total: files + callers_total,
                shown: groups.iter().map(|g| g.len()).sum::<usize>() + callers,
                ..r.outcome.clone()
            });
            if c.json_greeg() {
                crate::json_native::impact(&mut w, &r, defs, caps, callers, files, &oc)?;
                return Ok((w, oc));
            }
            let grp = |files: &[verbs::ImpactFile]| -> Vec<serde_json::Value> {
                files.iter().map(|f| json!({"path":crate::json_rel(&f.rel),"hits":f.hits,"kinds":f.kinds.iter().map(|(k,n)| json!([k.name(), n])).collect::<Vec<_>>(),"file_flags":f.flags.names(),"sample":f.sample.iter().map(|(l, t)| json!([l, crate::json_rel(t)])).collect::<Vec<_>>()})).collect()
            };
            serde_json::to_writer(
                &mut w,
                &json!({"type":"impact","data":{"name":r.name,"definitions":r.defs[..defs].iter().map(def_json).collect::<Vec<_>>(),"will_break":grp(groups[0]),"may_break":grp(groups[1]),"review":grp(groups[2]),
                "callers":r.callers.callers.iter().take(callers).map(|cl| json!({"path":crate::json_rel(&cl.rel),"symbol":chain_str(&cl.chain),"count":cl.count,"called_by":cl.called_by})).collect::<Vec<_>>(),
                "total_hits":r.total_hits,"elapsed_ms":r.elapsed_ms}}),
            )?;
            writeln!(w)?;
            serde_json::to_writer(
                &mut w,
                &json!({"type":"footer","data":{"verb":"impact","name":r.name,"definitions_total":r.defs_total,"files":files,"callers_total":callers_total,"total_hits":r.total_hits,"elapsed_ms":r.elapsed_ms,"outcome":outcome_json(&oc)}}),
            )?;
            writeln!(w)?;
            Ok((w, oc))
        };
        let max = [
            r.likely.len(),
            r.possible.len(),
            r.review.len(),
            r.defs.len(),
            callers_shown,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let fitted = fit(c.json(), o, max, render)?;
        return finish_fit(c, o, w, "impact", ex.as_ref(), fitted);
    }
    if r.total_hits == 0 {
        let oc = outcome(0);
        let render = |_: Cut| -> Result<(Vec<u8>, Outcome)> {
            let mut w = Vec::new();
            writeln!(w, "impact {}  no references found", r.name)?;
            outcome_line(&mut w, &[], &oc, MORE)?;
            Ok((w, oc.clone()))
        };
        let fitted = fit(c.json(), o, 0, render)?;
        return finish_fit(c, o, w, "impact", ex.as_ref(), fitted);
    }
    let likely = format!("uses {} and imports a file that defines it", r.name);
    let possible = format!("uses {} without that import link", r.name);
    let render = |cut: Cut| -> Result<(Vec<u8>, Outcome)> {
        let per_group = cut.rows;
        let mut w = Vec::new();
        let caps = caps(per_group);
        let callers_shown = callers_total.min(per_group);
        let files_shown: usize = groups
            .iter()
            .zip(caps)
            .map(|(g, cap)| g.len().min(cap))
            .sum();
        let oc = cut.mark(Outcome {
            total: files + callers_total,
            shown: files_shown + callers_shown,
            ..r.outcome.clone()
        });
        writeln!(
            w,
            "impact {}  {} hits · {} files · {} definitions{}{}{}",
            r.name,
            fmt_n(r.total_hits),
            files,
            r.defs_total,
            relaxed_note(&r.outcome.rung),
            if r.import_graph {
                ""
            } else {
                " · no import graph: no file is likely affected"
            },
            ms(c, r.elapsed_ms)
        )?;
        let defs: Vec<&DefEntry> = r.defs.iter().take(per_group.min(3)).collect();
        write_def_groups(&mut w, &defs, false, false, c.chain, None)?;
        write_group(&mut w, "LIKELY AFFECTED", &likely, &r.likely, caps[0])?;
        write_group(&mut w, "POSSIBLE", &possible, &r.possible, caps[1])?;
        write_group(
            &mut w,
            "REVIEW",
            "tests, comments, strings, demoted files",
            &r.review,
            caps[2],
        )?;
        if !r.callers.callers.is_empty() {
            writeln!(
                w,
                "\ncallers by name ({} functions, depth 2)",
                r.callers.callers.len()
            )?;
            let callers: Vec<&verbs::Caller> =
                r.callers.callers.iter().take(callers_shown).collect();
            let mut order: Vec<&[u8]> = Vec::new();
            for cl in &callers {
                if !order.contains(&cl.rel.as_slice()) {
                    order.push(&cl.rel);
                }
            }
            for rel in order {
                writeln!(w, "{}", path_text(rel))?;
                for cl in callers.iter().filter(|cl| cl.rel == rel) {
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
            if callers_total > callers_shown {
                writeln!(w, "  +{} more", callers_total - callers_shown)?;
            }
        }
        outcome_line(
            &mut w,
            &[
                (files_shown, files, "files"),
                (callers_shown, callers_total, "callers"),
            ],
            &oc,
            cut.more(),
        )?;
        Ok((w, oc))
    };
    let fitted = fit(c.json(), o, per_group, render)?;
    finish_fit(c, o, w, "impact", ex.as_ref(), fitted)
}
