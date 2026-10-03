//! C6: search a readable, non-tty stdin the way ripgrep does when no path is
//! given. The result is an ordinary `ScanResult` with one file named
//! `<stdin>` so the parity renderers (text and JSON) apply unchanged.

use crate::{
    CollectSink, FileResult, Hit, HitKind, Mode, Options, Rung, ScanResult, Source, Stats,
    build_matcher,
};
use anyhow::Result;
use greeg_lang::{FileFlags, Lang};
use grep_searcher::SearcherBuilder;
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
pub fn scan(o: &Options, mut data: Vec<u8>) -> Result<ScanResult> {
    let t0 = Instant::now();
    // Same as `process_file`: transcode BOM-marked UTF-16 and search past a
    // UTF-8 BOM while offsets keep indexing the whole buffer.
    let encoding = crate::Encoding::sniff(&data);
    greeg_lang::transcode_utf16(&mut data);
    let bom = if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let matcher = build_matcher(o)?;
    let mut sink = CollectSink::new(&matcher, Lang::None, usize::MAX, false, o.multiline);
    sink.base = bom as u32;
    sink.first_only = o.mode == Mode::Files;
    let extent = crate::text_extent(&data[bom..]);
    let (end, nul) = extent.unwrap_or((0, None));
    let mut sb = SearcherBuilder::new();
    sb.line_number(true)
        .multi_line(o.multiline)
        .bom_sniffing(false);
    sb.build()
        .search_slice(&matcher, &data[bom..bom + end], &mut sink)?;
    let searched = end as u64;
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
            bom: bom as u32,
            searched,
            binary_offset: nul.map(|p| (bom + p) as u64),
            encoding,
            below: None,
        });
    }
    let mut stats = Stats {
        files_walked: 1,
        files_searched: 1,
        bytes_searched: searched,
        skipped_binary: usize::from(extent.is_none()),
        binary_tails: usize::from(nul.is_some()),
        files_matched: files.len(),
        total_hits: total,
        total_unfiltered: total,
        threads: 1,
        source: "stdin",
        ..Default::default()
    };
    stats.by_kind[HitKind::Ident.idx()] = total;
    stats.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(ScanResult {
        opts: o.clone(),
        files,
        stats,
        rung: Rung::Exact,
        ignored_only: None,
        ignored_partial: false,
        related_index: Vec::new(),
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
        assert_eq!(r.files[0].rel, STDIN_NAME.as_bytes());
        assert_eq!(
            r.files[0].hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(r.files[0].hits[1].raw, b"xa");
        let r = scan(&o, b"b\n".to_vec()).unwrap();
        assert!(r.files.is_empty());
    }

    #[test]
    fn stdin_scan_skips_utf8_bom() {
        let o = Options {
            pattern: "^foo".into(),
            budget: 0,
            ..Default::default()
        };
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"foo bar\nfoo baz\n");
        let r = scan(&o, data).unwrap();
        assert_eq!(r.stats.total_hits, 2);
        let hits = &r.files[0].hits;
        assert_eq!(hits.iter().map(|h| h.line).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(hits[0].raw, b"foo bar");
    }

    #[test]
    fn stdin_scan_transcodes_utf16() {
        let o = Options {
            pattern: "foo".into(),
            budget: 0,
            ..Default::default()
        };
        let mut data = vec![0xFF, 0xFE];
        data.extend("foo\nbar\n".encode_utf16().flat_map(u16::to_le_bytes));
        let r = scan(&o, data).unwrap();
        assert_eq!(r.stats.total_hits, 1);
        assert_eq!(r.files[0].hits[0].line, 1);
        assert_eq!(r.stats.skipped_binary, 0);
    }
}
