//! `--precise` (DESIGN.md §3.4): re-parse the files that will be shown and
//! replace the byte-rule kinds (call/type/member/ident) with the syntax
//! tree's answer. Costs one parse per shown file; bounded by the budget
//! through the number of shown files.

use crate::shape::Report;
use crate::{HitKind, ScanResult};
use greeg_lang::sym::{self, NodeKind};

pub fn apply(r: &mut ScanResult, rep: &Report) {
    let mut files: Vec<usize> = rep.files.iter().map(|f| f.file).collect();
    files.sort_unstable();
    files.dedup();
    for fi in files {
        let f = &mut r.files[fi];
        if !f.lang.has_grammar() {
            continue;
        }
        if !f.hits.iter().any(|h| matches!(h.kind, HitKind::Call | HitKind::Type | HitKind::Member | HitKind::Ident)) {
            continue;
        }
        let Some(src) = f.source().map(|s| s.bytes.clone()) else { continue };
        let Some(parsed) = sym::parse(f.lang, sym::is_tsx(&f.rel), &src) else { continue };
        for h in &mut f.hits {
            if !matches!(h.kind, HitKind::Call | HitKind::Type | HitKind::Member | HitKind::Ident) || h.match_end as usize > src.len() {
                continue;
            }
            let old = h.kind;
            let new = match parsed.kind_at(h.match_start as usize, h.match_end as usize) {
                NodeKind::Call => HitKind::Call,
                NodeKind::Type => HitKind::Type,
                NodeKind::Member => HitKind::Member,
                NodeKind::Ident => HitKind::Ident,
                NodeKind::Def => HitKind::Def,
                NodeKind::Import => HitKind::Import,
                NodeKind::Comment => HitKind::Comment,
                NodeKind::Str => HitKind::Str,
            };
            if new != old {
                f.kinds[old.idx()] = f.kinds[old.idx()].saturating_sub(1);
                f.kinds[new.idx()] += 1;
                h.score = h.score / old.weight() * new.weight();
                h.kind = new;
            }
        }
    }
}
