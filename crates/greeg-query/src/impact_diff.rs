//! `greeg impact --diff`: the definitions a checkout changed against a git
//! base, and the places that use them by name. A use found by name is a
//! candidate: same-named definitions elsewhere are listed, not resolved.

use crate::change::{ChangedDef, Changes, DefChange, DiffSpec, changes};
use crate::{HitKind, MatchingPolicy, Mode, Options, scan, verbs};
use anyhow::Result;
use greeg_lang::{DefKind, FileFlags};
use std::time::Instant;

/// Names searched per answer; breaking changes first.
pub const MAX_NAMES: usize = 16;
/// Sites kept per definition; more are counted.
const MAX_SITES: usize = 400;

#[derive(Clone, Debug)]
pub struct Site {
    /// Root-relative path bytes (`greeg_index::rel`).
    pub rel: Vec<u8>,
    pub line: u32,
    pub kind: HitKind,
    /// Enclosing definitions, outermost first.
    pub chain: Vec<(DefKind, String)>,
    pub text: Vec<u8>,
    pub flags: FileFlags,
}

#[derive(Clone, Debug)]
pub struct DefImpact {
    /// Into `Changes::files`, and that file's `defs`.
    pub file: usize,
    pub def: usize,
    /// Direct uses (calls, imports, type uses) first, then the others.
    pub sites: Vec<Site>,
    pub sites_total: usize,
    /// Of `sites_total`, the direct uses; a function's member access without
    /// a call may be of a same-named field.
    pub direct_total: usize,
    pub files_total: usize,
    /// Uses on lines the change itself edited.
    pub edited: usize,
    /// Other definitions with the name: (path, line, kind).
    pub others: Vec<(Vec<u8>, u32, DefKind)>,
    pub others_total: usize,
    /// An earlier impact with the same old name (an `expect` and its
    /// `actual`s, overloads): its uses are these, not repeated.
    pub same_as: Option<usize>,
}

pub struct DiffImpactResult {
    pub changes: Changes,
    pub impacts: Vec<DefImpact>,
    /// Changed definitions whose uses were not searched (past [`MAX_NAMES`]).
    pub unsearched: usize,
    pub source: &'static str,
    pub fresh: &'static str,
    pub elapsed_ms: f64,
}

impl DiffImpactResult {
    pub fn def(&self, i: &DefImpact) -> &ChangedDef {
        &self.changes.files[i.file].defs[i.def]
    }
    pub fn defs_total(&self) -> usize {
        self.changes.files.iter().map(|f| f.defs.len()).sum()
    }
}

/// The kinds of use that can depend on a definition of `kind`, strongest first.
fn use_kinds(kind: DefKind) -> &'static [HitKind] {
    match kind {
        DefKind::Function | DefKind::Method | DefKind::Macro => {
            &[HitKind::Call, HitKind::Import, HitKind::Member]
        }
        k if k.is_container() || k == DefKind::TypeAlias => &[
            HitKind::Type,
            HitKind::Call,
            HitKind::Import,
            HitKind::Member,
        ],
        _ => &[
            HitKind::Member,
            HitKind::Ident,
            HitKind::Call,
            HitKind::Import,
            HitKind::Type,
        ],
    }
}

/// A use the change breaks or may break: any use of a removed or renamed
/// name but a member access of a callable (likely a same-named field), and
/// the calls and type uses of a changed signature (an import stays valid).
/// A body change breaks none: its callers are to recheck.
pub fn direct(def: &ChangedDef, k: HitKind) -> bool {
    if def.change == DefChange::Body {
        return false;
    }
    let callable = matches!(
        def.kind,
        DefKind::Function | DefKind::Method | DefKind::Macro
    );
    let typed = def.kind.is_container() || def.kind == DefKind::TypeAlias;
    match k {
        HitKind::Call | HitKind::Type => true,
        HitKind::Import => matches!(def.change, DefChange::Removed | DefChange::Renamed { .. }),
        _ => !(callable || typed),
    }
}

/// Whether byte `col` of `line` sits in a string's interpolated expression:
/// `${…}` (Kotlin, JavaScript, TypeScript) or `{…}` in a Python f-string.
fn interpolated(line: &[u8], col: usize) -> bool {
    let before = &line[..col.min(line.len())];
    let open = |at: usize| !before[at..].contains(&b'}');
    if let Some(p) = before.windows(2).rposition(|w| w == b"${") {
        return open(p);
    }
    let Some(p) = before.iter().rposition(|&c| c == b'{') else {
        return false;
    };
    let head = &before[..p];
    open(p)
        && head
            .iter()
            .rposition(|&c| c == b'"' || c == b'\'')
            .is_some_and(|q| q > 0 && matches!(head[q - 1], b'f' | b'F'))
}

/// A string hit in an interpolation is code: a call when `(` follows.
fn code_kind(h: &crate::Hit) -> Option<HitKind> {
    if h.kind != HitKind::Str {
        return Some(h.kind);
    }
    let col = h.column() as usize;
    if !interpolated(&h.raw, col) {
        return None;
    }
    let end = (h.match_end - h.line_start) as usize;
    let rest = h.raw.get(end..).unwrap_or_default();
    Some(if rest.trim_ascii_start().starts_with(b"(") {
        HitKind::Call
    } else {
        HitKind::Ident
    })
}

pub fn impact_diff(o: &Options, spec: &DiffSpec) -> Result<DiffImpactResult> {
    let t0 = Instant::now();
    let changes = changes(&o.root, spec)?;
    // breaking changes first, then body changes; added definitions have no
    // uses to break
    let mut order: Vec<(usize, usize)> = Vec::new();
    for breaking in [true, false] {
        for (fi, f) in changes.files.iter().enumerate() {
            for (di, d) in f.defs.iter().enumerate() {
                if d.change != DefChange::Added && d.change.breaking() == breaking {
                    order.push((fi, di));
                }
            }
        }
    }
    let unsearched = order.len().saturating_sub(MAX_NAMES);
    order.truncate(MAX_NAMES);
    // The searches below skip the freshness check, so the changed files'
    // deltas are applied first: a search defers them to its own answer
    // only. Without a usable index every search scans.
    let threads = if o.threads == 0 {
        crate::default_threads()
    } else {
        o.threads
    };
    let opened = if o.use_index && !order.is_empty() {
        crate::indexed::open_fresh(o, threads)?
    } else {
        None
    };
    let (source, fresh) = match &opened {
        Some(op) => ("index", op.fresh_method),
        None => ("scan", ""),
    };
    let use_index = opened.is_some();
    drop(opened);
    let mut impacts: Vec<DefImpact> = Vec::with_capacity(order.len());
    for (fi, di) in order {
        let d = &changes.files[fi].defs[di];
        let name = d.old_name().to_string();
        let first = impacts.iter().position(|i| {
            let e = &changes.files[i.file].defs[i.def];
            e.old_name() == name && e.change.name() == d.change.name()
        });
        if let Some(k) = first {
            let i = &impacts[k];
            impacts.push(DefImpact {
                file: fi,
                def: di,
                sites: Vec::new(),
                same_as: Some(k),
                others: i.others.clone(),
                ..*i
            });
            continue;
        }
        let mut so = o.clone();
        so.pattern = name.clone();
        so.fixed_strings = true;
        so.word = true;
        so.case_insensitive = false;
        so.smart_case = false;
        so.matching = MatchingPolicy::Exact;
        // a string hit may be an interpolated expression (`"${f()}"`)
        so.kinds = [use_kinds(d.kind), &[HitKind::Str]].concat();
        so.mode = Mode::Content;
        so.budget = 0;
        so.use_index = use_index;
        so.fresh = greeg_index::fresh::Mode::None;
        let mut r = scan(&so)?;
        let idxs: Vec<usize> = (0..r.files.len().min(400)).collect();
        crate::refine(&mut r, &idxs);
        let (mut sites, mut weak) = (Vec::new(), Vec::new());
        let (mut total, mut direct_total, mut edited, mut files) = (0usize, 0usize, 0usize, 0usize);
        for f in &r.files {
            let changed = changes.files.iter().find(|c| c.rel == f.rel);
            let mut counted = false;
            for h in &f.hits {
                let Some(kind) = code_kind(h).filter(|k| use_kinds(d.kind).contains(k)) else {
                    continue;
                };
                if changed.is_some_and(|c| c.changed_line(h.line)) {
                    edited += 1;
                    continue;
                }
                total += 1;
                if !counted {
                    files += 1;
                    counted = true;
                }
                let site = Site {
                    rel: f.rel.clone(),
                    line: h.line,
                    kind,
                    chain: h.chain.to_vec(),
                    text: h.display(so.max_columns).0.into_owned(),
                    flags: f.flags,
                };
                if direct(d, kind) {
                    direct_total += 1;
                    sites.push(site);
                } else {
                    weak.push(site);
                }
            }
        }
        sites.append(&mut weak);
        sites.truncate(MAX_SITES);
        let (others, others_total) = other_definitions(&so, &name, &changes, fi, d);
        impacts.push(DefImpact {
            file: fi,
            def: di,
            sites,
            sites_total: total,
            direct_total,
            files_total: files,
            edited,
            others,
            others_total,
            same_as: None,
        });
    }
    Ok(DiffImpactResult {
        changes,
        impacts,
        unsearched,
        source,
        fresh,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
    })
}

/// Definitions named `name` other than the changed one: a use found by name
/// may belong to any of them.
fn other_definitions(
    o: &Options,
    name: &str,
    changes: &Changes,
    fi: usize,
    d: &ChangedDef,
) -> (Vec<(Vec<u8>, u32, DefKind)>, usize) {
    let mut q = o.clone();
    q.kinds.clear();
    q.budget = 400;
    let Ok(found) = verbs::def(&q, name, &[], None) else {
        return (Vec::new(), 0);
    };
    let file = &changes.files[fi];
    let own = |e: &verbs::DefEntry| {
        e.rel == file.rel && e.line == d.line && !matches!(d.change, DefChange::Removed)
    };
    let others: Vec<_> = found
        .entries
        .iter()
        .filter(|e| !own(e) && !e.file_module)
        .map(|e| (e.rel.clone(), e.line, e.kind))
        .collect();
    let mine = found.entries.iter().filter(|e| own(e)).count();
    let total = found.total.saturating_sub(mine).max(others.len());
    (others, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolations_are_code() {
        let at = |s: &str, w: &str| interpolated(s.as_bytes(), s.find(w).unwrap());
        assert!(at(r#""a ${x.f()} b""#, "f()"));
        assert!(at("`${obj.f(1)}`", "f(1)"));
        assert!(at(r#"f"value {f(x)}""#, "f(x)"));
        assert!(!at(r#""a ${x} f() b""#, "f()"));
        assert!(!at(r#""call f() here""#, "f()"));
        assert!(!at(r#""{f}""#, "f}"));
    }
}
