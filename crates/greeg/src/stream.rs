//! `--budget 0` written as it is found: each matched file is shaped and
//! written once the files before it are, so memory is bounded by the files in
//! flight, not by the matches.

use crate::{
    Common, Fmt, LegacyPrinted, RgPrinted, json_native, legacy_bytes_searched, legacy_file,
    legacy_tail, render_footer, render_parity_file, rg_json_file, rg_json_summary, stats,
    verbs_out,
};
use anyhow::Result;
use greeg_query::outcome::Outcome;
use greeg_query::session::Session;
use greeg_query::shape;
use greeg_query::{FileResult, MatchingPolicy, Mode, Options, Rung, ScanResult};
use std::collections::HashSet;
use std::io::{BufWriter, Write};
use std::time::Instant;

/// An unbudgeted content answer streams: every match, in path order, with
/// nothing that needs the whole answer before its first byte.
pub(crate) fn applies(c: &Common, o: &Options) -> bool {
    o.budget == 0
        && o.mode == Mode::Content
        && o.max_bytes == 0
        && !c.explain
        && o.matching == MatchingPolicy::Exact
}

#[derive(Clone, Copy)]
enum Format {
    Text,
    Rg,
    Legacy,
    Native,
}

/// What the files written so far add up to.
#[derive(Default)]
struct Written {
    files: usize,
    hits: usize,
    /// Each file's shaped estimate, footer included, and how many there were.
    est: usize,
    shaped: usize,
    first_file: bool,
    rg: RgPrinted,
    legacy: LegacyPrinted,
    /// For the session: the first files shown, context ranges, new files.
    keys: Vec<String>,
    ranges: Vec<(String, u32, u32, u64)>,
    new_files: usize,
}

pub(crate) fn run(
    c: &Common,
    o: &Options,
    fmt: Fmt,
    session: Option<&Session>,
    t0: Instant,
) -> Result<()> {
    greeg_index::commit::stream();
    let format = if !c.json() {
        Format::Text
    } else if c.json_rg() {
        Format::Rg
    } else if c.json_greeg() {
        Format::Native
    } else {
        Format::Legacy
    };
    let repeats = session.map_or(0, |s| s.repeats(o));
    let seen: Option<HashSet<String>> = session.filter(|_| repeats >= 2).map(|s| s.seen_files());
    let stdout = std::io::stdout();
    let mut w = BufWriter::with_capacity(64 * 1024, stats::Tee(stdout.lock()));
    if let Format::Native = format {
        json_native::header(&mut w, "search")?;
    }
    let mut done = Written {
        first_file: true,
        ..Default::default()
    };
    // one result, reused: each file is shaped alone
    let mut one = ScanResult {
        opts: o.clone(),
        files: Vec::with_capacity(1),
        stats: Default::default(),
        rung: Rung::Exact,
        ignored_only: None,
        ignored_partial: false,
        related_index: Vec::new(),
    };
    let mut sink = |fr: FileResult| -> Result<()> {
        one.files.clear();
        one.files.push(fr);
        let rep = shape::shape(&mut one);
        if o.precise {
            greeg_query::precise::apply(&mut one, &rep);
        }
        done.shaped += 1;
        done.est += rep.footer.est_tokens;
        done.files += rep.files.len();
        done.hits += rep.footer.hits_shown;
        for sf in &rep.files {
            let f = &one.files[sf.file];
            match format {
                Format::Text => render_parity_file(&mut w, f, sf, fmt, &mut done.first_file)?,
                Format::Rg => rg_json_file(&mut w, &one, sf, &mut done.rg)?,
                Format::Legacy => {
                    done.legacy.searched += legacy_bytes_searched(f);
                    legacy_file(&mut w, &one, sf, &mut done.legacy)?;
                }
                Format::Native => json_native::search_file(&mut w, &one, rep.layout, sf)?,
            }
            if session.is_some() {
                let key = greeg_index::rel::key(&f.rel).into_owned();
                if seen.as_ref().is_some_and(|s| !s.contains(&key)) {
                    done.new_files += 1;
                }
                for sh in &sf.hits {
                    if let Some((first, lines)) = sh.context.as_ref().filter(|(_, l)| !l.is_empty())
                        && done.ranges.len() < 128
                    {
                        let last = first + lines.len() as u32 - 1;
                        done.ranges.push((key.clone(), *first, last, f.mtime));
                    }
                }
                if done.keys.len() < 64 && done.keys.last() != Some(&key) {
                    done.keys.push(key);
                }
            }
        }
        Ok(())
    };
    let mut result = greeg_query::scan_streamed_exact(o, &mut sink)?;
    let t_scan = t0.elapsed();
    // the counts come from the scan; what was shown, from the files written
    let mut report = shape::shape(&mut result);
    let base = report.footer.est_tokens;
    report.footer.est_tokens = base + (done.est - done.shaped * base);
    report.footer.files_shown = done.files;
    report.footer.hits_shown = done.hits;
    if let Some(s) = session {
        s.loop_hint_counted(o, repeats, done.new_files, &mut report);
    }
    let mut err = Vec::new();
    match format {
        Format::Text => render_footer(
            &mut err,
            &result,
            &report,
            report.footer.est_tokens,
            fmt,
            false,
        )?,
        Format::Rg => rg_json_summary(&mut w, &result.stats, &done.rg)?,
        Format::Legacy => legacy_tail(&mut w, &result, &report, &done.legacy, Instant::now())?,
        Format::Native => json_native::search_footer(&mut w, &result, &report)?,
    }
    w.flush()?;
    drop(w);
    if !err.is_empty() {
        std::io::stderr().write_all(&err)?;
        stats::observe(&err);
    }
    if let Some(s) = session {
        s.record_parts(o, result.stats.total_hits, done.keys, done.ranges);
    }
    if c.stats {
        let s = &result.stats;
        eprintln!(
            "greeg: {} · walked {} files, candidates {}, searched {}, matched {} ({} hits) with {} threads; binary {} tails {} huge {}",
            s.source,
            s.files_walked,
            s.candidates,
            s.files_searched,
            s.files_matched,
            s.total_hits,
            s.threads,
            s.skipped_binary,
            s.binary_tails,
            s.skipped_huge
        );
        if s.source == "index" {
            eprintln!(
                "greeg: fresh {} {:.1} ms ({} changed) · plan {}",
                s.fresh_method, s.fresh_ms, s.fresh_changed, s.plan
            );
        }
        eprintln!(
            "greeg: streamed {:.1} ms · total {:.1} ms · cpu: read {:.1} search {:.1} classify {:.1} ms",
            t_scan.as_secs_f64() * 1e3,
            t0.elapsed().as_secs_f64() * 1e3,
            s.cpu_read_ms,
            s.cpu_search_ms,
            s.cpu_classify_ms
        );
    }
    verbs_out::finish_run(
        stats::RunInfo {
            verb: "search",
            scan_ms: Some(t_scan.as_secs_f64() * 1e3),
            shape_ms: None,
            source: Some(result.stats.source.to_string()),
            hits: Some(result.stats.total_hits),
            files: Some(result.stats.files_matched),
            exit: 0,
        },
        &Outcome::of_answer(&result, &report.footer),
    )
}
