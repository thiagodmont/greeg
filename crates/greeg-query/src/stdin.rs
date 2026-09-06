//! C6: search a readable, non-tty stdin the way ripgrep does when no path is
//! given. The result is an ordinary `ScanResult` with one file named
//! `<stdin>` so the parity renderers (text and JSON) apply unchanged.

use crate::{
    CollectSink, FileResult, Hit, HitKind, Mode, Options, Rung, ScanResult, Source, Stats,
    build_matcher, clip_line,
};
use anyhow::Result;
use greeg_lang::{FileFlags, Lang};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use std::time::Instant;

pub const STDIN_NAME: &str = "<stdin>";

/// Mirror of `grep_cli::is_readable_stdin`: stdin is not a terminal and is a
/// regular file (with data), a pipe or a socket.
pub fn is_readable_stdin() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: fstat on fd 0 writes a stat buffer; isatty reads a flag.
        unsafe {
            if libc::isatty(0) == 1 {
                return false;
            }
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(0, &mut st) != 0 {
                return false;
            }
            let ft = st.st_mode & libc::S_IFMT;
            (ft == libc::S_IFREG && st.st_size > 0) || ft == libc::S_IFIFO || ft == libc::S_IFSOCK
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Search `data` (the whole of stdin) with the user's pattern and flags.
pub fn scan(o: &Options, data: Vec<u8>) -> Result<ScanResult> {
    let t0 = Instant::now();
    let matcher = build_matcher(o)?;
    let mut sink = CollectSink::new(&matcher, Lang::None, usize::MAX, false, o.multiline);
    sink.first_only = o.mode == Mode::Files;
    let mut sb = SearcherBuilder::new();
    sb.line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .multi_line(o.multiline)
        .bom_sniffing(false);
    sb.build().search_slice(&matcher, &data, &mut sink)?;
    let src = Source::new(data);
    let bytes: &[u8] = &src.bytes;
    let mut hits = Vec::with_capacity(sink.hits.len());
    for lh in sink.hits {
        let ls = lh.line_start;
        let (ms, me) = lh.subs[0];
        let le = memchr::memchr(b'\n', &bytes[ls as usize..])
            .map(|k| ls as usize + k)
            .unwrap_or(bytes.len());
        let line_bytes = &bytes[ls as usize..le];
        let me_line = (me as usize).min(le) as u32;
        let (text, clipped, tm) = clip_line(
            line_bytes,
            (ms - ls) as usize,
            (me_line - ls) as usize,
            o.max_columns,
        );
        let raw = line_bytes
            .strip_suffix(b"\r")
            .unwrap_or(line_bytes)
            .to_vec();
        hits.push(Hit {
            line: lh.line,
            line_start: ls,
            match_start: ms,
            match_end: me,
            submatches: lh.subs,
            kind: HitKind::Ident,
            chain: Vec::new(),
            def_idx: None,
            score: 1.0,
            exact: crate::is_exact(o, bytes, ms, me),
            text,
            text_match: (tm.0 as u32, tm.1 as u32),
            clipped,
            raw,
        });
    }
    let total = sink.total;
    let mut kinds = [0u32; 9];
    kinds[HitKind::Ident.idx()] = hits.len() as u32;
    let mut files = Vec::new();
    if total > 0 {
        files.push(FileResult {
            rel: STDIN_NAME.into(),
            path: STDIN_NAME.into(),
            lang: Lang::None,
            flags: FileFlags::default(),
            size: bytes.len() as u64,
            age_days: 0.0,
            mtime: 0,
            prior: 1.0,
            hits,
            total,
            total_unfiltered: total,
            kinds,
            defs: Vec::new(),
            refined: true,
            file_id: None,
            src: Some(src),
        });
    }
    let mut stats = Stats {
        files_walked: 1,
        files_searched: 1,
        files_matched: files.len(),
        total_hits: total,
        total_unfiltered: total,
        threads: 1,
        source: "stdin",
        ..Default::default()
    };
    stats.by_kind[HitKind::Ident.idx()] = total;
    stats.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    if sink.binary {
        stats.skipped_binary = 1;
    }
    Ok(ScanResult {
        opts: o.clone(),
        files,
        stats,
        rung: Rung::Exact,
        ignored_only: None,
        ignored_partial: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_scan_lines() {
        let o = Options {
            pattern: "a".into(),
            budget: 0,
            ..Default::default()
        };
        let r = scan(&o, b"a\nb\nxa\n".to_vec()).unwrap();
        assert_eq!(r.stats.total_hits, 2);
        assert_eq!(r.files[0].rel, STDIN_NAME);
        assert_eq!(
            r.files[0].hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(r.files[0].hits[1].raw, b"xa");
        let r = scan(&o, b"b\n".to_vec()).unwrap();
        assert!(r.files.is_empty());
    }
}
