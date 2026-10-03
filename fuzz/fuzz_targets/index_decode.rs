//! An index component read from any bytes either fails to parse or can be
//! walked through the ids it holds without a panic. Input: a selector byte
//! (which component: files, grams, words, symbols, spans, graph, a delta's
//! graph, skipped), then its body.
#![no_main]

use greeg_index::format::{FilesView, GramsView};
use greeg_index::skipped::Skipped;
use greeg_index::symtab::{DeltaGraphView, GraphView, SpansView, SymbolsView};
use greeg_index::words::WordsView;
use libfuzzer_sys::fuzz_target;
use roaring::RoaringBitmap;

/// Enough of each table to reach every decoder path without slowing a run.
const MAX: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let Some((&which, body)) = data.split_first() else {
        return;
    };
    // a component's body starts 8-aligned in its mapped file
    let mut words = vec![0u64; body.len().div_ceil(8)];
    bytemuck::cast_slice_mut::<u64, u8>(&mut words)[..body.len()].copy_from_slice(body);
    let body = &bytemuck::cast_slice::<u64, u8>(&words)[..body.len()];
    match which % 8 {
        0 => {
            let Ok(v) = FilesView::parse(body, None) else {
                return;
            };
            for i in 0..v.len().min(MAX) {
                if let Some(r) = v.rec(i) {
                    let _ = v.path(r);
                }
            }
            for d in v.dirs().iter().take(MAX) {
                let _ = v.dir_path(d);
            }
            let _ = (v.huge(), v.hidden(), v.arena());
        }
        1 => {
            let Ok(v) = GramsView::parse(body, None) else {
                return;
            };
            for i in 0..v.len().min(MAX) {
                let _ = v.count(i);
                let _ = RoaringBitmap::deserialize_from(v.posting_bytes(i));
            }
            let _ = v.find(u32::from_le_bytes(*b"abc\0"));
        }
        2 => {
            let Ok(v) = WordsView::parse(body, None) else {
                return;
            };
            for i in 0..v.len().min(MAX) {
                let w = v.word(i);
                let _ = (v.find(w), v.count(i));
                let _ = RoaringBitmap::deserialize_from(v.posting_bytes(i));
            }
            if let Some((word_off, _, _)) = v.dictionary() {
                let _ = greeg_index::words::word_at_offset(word_off, body.len() / 2);
            }
        }
        3 => {
            let Ok(v) = SymbolsView::parse(body, None) else {
                return;
            };
            for s in v.all_syms().iter().take(MAX) {
                let name = v.name(s.name_id);
                let _ = (v.find_name(name), v.prefix_range(name), v.supers_of(s));
                let _ = v.syms_named(s.name_id);
                let _ = v.symbols_of(s.file);
                let _ = v.enclosing(s.file, s.start);
            }
        }
        4 => {
            let Ok(v) = SpansView::parse(body, None) else {
                return;
            };
            for f in 0..64 {
                for n in v.noncode_of(f).iter().take(MAX) {
                    let _ = v.noncode_at(f, n.end());
                }
                for i in v.imports_of(f).iter().take(MAX) {
                    let _ = (v.raw(i), v.import_at(f, i.start));
                }
            }
        }
        5 => {
            let Ok(v) = GraphView::parse(body) else {
                return;
            };
            for f in 0..1024 {
                for &t in v.out(f).iter().take(MAX) {
                    let _ = v.incoming(t);
                }
            }
        }
        6 => {
            let Ok(v) = DeltaGraphView::parse(body) else {
                return;
            };
            for f in 0..v.n.min(MAX as u32) {
                let _ = (v.prev(f), v.out(f));
            }
            let _ = v.edges().take(MAX).count();
        }
        _ => {
            // read from its file without a digest: the header too
            let body = greeg_index::format::check_header(body, greeg_index::format::COMP_SKIPPED)
                .unwrap_or(body);
            if let Ok(s) = Skipped::parse(body) {
                let _ = s.entries().take(MAX).count();
                let _ = s.serialize();
            }
        }
    }
});
