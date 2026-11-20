//! `greeg impact --diff`: the definitions a checkout changed against a git
//! base, and the places that use them by name. A use found by name is a
//! candidate: same-named definitions elsewhere are listed, not resolved.

use crate::change::{ChangedDef, Changes, DefChange, DiffSpec, changes};
use crate::{Hit, HitKind, MatchingPolicy, Mode, Options, ScanKinds, scan, verbs};
use anyhow::Result;
use greeg_lang::{DefKind, FileFlags, Lang};
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
    /// Direct uses (calls, imports, type uses) first, then names used as
    /// values, then the others.
    pub sites: Vec<Site>,
    pub sites_total: usize,
    /// Of `sites_total`, the direct uses; a function's member access without
    /// a call may be of a same-named field.
    pub direct_total: usize,
    /// Of `direct_total`, the names used as values; counted before `sites`
    /// is cut.
    pub values_total: usize,
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
/// A removed or renamed function, macro or type also counts its name used as
/// a value; a method is never named by a bare identifier.
fn use_kinds(kind: DefKind, change: &DefChange) -> &'static [HitKind] {
    let gone = matches!(change, DefChange::Removed | DefChange::Renamed { .. });
    match kind {
        DefKind::Function | DefKind::Macro if gone => &[
            HitKind::Call,
            HitKind::Import,
            HitKind::Member,
            HitKind::Ident,
        ],
        DefKind::Function | DefKind::Method | DefKind::Macro => {
            &[HitKind::Call, HitKind::Import, HitKind::Member]
        }
        k if (k.is_container() || k == DefKind::TypeAlias) && gone => &[
            HitKind::Type,
            HitKind::Call,
            HitKind::Import,
            HitKind::Member,
            HitKind::Ident,
        ],
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
        HitKind::Ident if callable || typed => is_value_use(def, k),
        _ => !(callable || typed),
    }
}

/// A removed or renamed function, macro or type named as a value (a callback
/// argument, a list element, `isinstance(x, T)`, a unit struct expression).
/// It breaks like a call, but is found by name only.
pub fn is_value_use(def: &ChangedDef, k: HitKind) -> bool {
    k == HitKind::Ident
        && matches!(def.change, DefChange::Removed | DefChange::Renamed { .. })
        && (matches!(def.kind, DefKind::Function | DefKind::Macro)
            || def.kind.is_container()
            || def.kind == DefKind::TypeAlias)
}

/// Whether the name at `start..end` of `line` declares a new local (`let x`,
/// `for x in`, `as x`, `x = 1`, a parameter) rather than using it.
fn binding(line: &[u8], start: usize, end: usize) -> bool {
    let start = start.min(line.len());
    let after = line.get(end..).unwrap_or_default().trim_ascii_start();
    // a pattern names a type: `let Point { x, .. } = p`, `let Wrap(x) = w`
    if matches!(after.first(), Some(b'{' | b'(')) || after.starts_with(b"::") {
        return false;
    }
    let before = line[..start].trim_ascii_end();
    let (rest, word) = last_word(before);
    let word = if word == b"mut" {
        last_word(rest.trim_ascii_end()).1
    } else {
        word
    };
    if matches!(word, b"let" | b"val" | b"var" | b"for" | b"as") {
        return true;
    }
    if after.first() == Some(&b'=') && !matches!(after.get(1), Some(b'=' | b'>')) {
        return true;
    }
    parameter(line, start)
}

/// `s` split before its trailing identifier.
fn last_word(s: &[u8]) -> (&[u8], &[u8]) {
    let at = s
        .iter()
        .rposition(|&c| !(c.is_ascii_alphanumeric() || c == b'_'))
        .map_or(0, |p| p + 1);
    s.split_at(at)
}

/// Whether byte `at` of `line` starts a parameter of the function the line
/// declares: right after `(` or `,` at the top level of its own parameter
/// list, not in a nested type, a return type or a one-line body.
fn parameter(line: &[u8], at: usize) -> bool {
    let Some(from) = declares_function(line) else {
        return false;
    };
    // `->` and `=>` are arrows, not closing brackets
    let arrow = |i: usize| i > 0 && matches!(line[i - 1], b'-' | b'=');
    // the list opens at the first `(` past the keyword, outside `<…>` generics
    let mut angle = 0usize;
    let mut open = None;
    for (i, &c) in line.iter().enumerate().skip(from) {
        match c {
            b'<' => angle += 1,
            b'>' if !arrow(i) => angle = angle.saturating_sub(1),
            b'(' if angle == 0 => {
                open = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(open) = open.filter(|&o| o < at) else {
        return false;
    };
    let (mut depth, mut prev) = (0usize, b'(');
    for (i, &c) in line.iter().enumerate().take(at).skip(open + 1) {
        match c {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b'>' if arrow(i) => {}
            b')' | b']' | b'}' | b'>' => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            prev = c;
        }
    }
    depth == 0 && matches!(prev, b'(' | b',')
}

/// Where the keyword ends when the statement keyword of `line` is `def`,
/// `fn`, `fun` or `function` (`function(`, `function*`), after any modifiers.
fn declares_function(line: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < line.len() {
        if line[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let s = i;
        while i < line.len() && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        match &line[s..i] {
            b"def" | b"fn" | b"fun" | b"function" => return Some(i),
            b"async" | b"export" | b"default" | b"const" | b"unsafe" | b"extern" | b"static"
            | b"public" | b"private" | b"protected" | b"internal" | b"override" | b"suspend"
            | b"inline" | b"open" | b"abstract" => {}
            w if w == b"pub" || w.starts_with(b"pub(") => {}
            w => {
                let word_byte = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$';
                return (w.starts_with(b"function") && !w.get(8).is_some_and(word_byte))
                    .then_some(s + 8);
            }
        }
    }
    None
}

/// The kind a line counts as when its first occurrence of the name binds a
/// new local: that of its first other occurrence that is code of a kind in
/// `wanted` and not itself a binding. `kinds` classifies an occurrence with
/// the file's strings and comments; without the file, the line alone does.
fn past_binding(
    h: &Hit,
    lang: Lang,
    kinds: Option<&ScanKinds>,
    wanted: &[HitKind],
) -> Option<HitKind> {
    let col = h.column();
    h.raw_submatches()
        .into_iter()
        .filter(|&(s, _)| s != col)
        .find_map(|(s, e)| {
            let (s, e) = (s as usize, e as usize);
            let k = match kinds {
                Some(kinds) => kinds.kind(
                    h.line_start + s as u32,
                    h.line_start + e as u32,
                    h.line_start,
                ),
                None => crate::kind_by_context(lang, &h.raw, s, e, 0, &h.raw),
            };
            let k = code_kind_at(k, &h.raw, s, e)?;
            (wanted.contains(&k) && !(k == HitKind::Ident && binding(&h.raw, s, e))).then_some(k)
        })
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
fn code_kind(h: &Hit) -> Option<HitKind> {
    let end = (h.match_end - h.line_start) as usize;
    code_kind_at(h.kind, &h.raw, h.column() as usize, end)
}

/// [`code_kind`] of the occurrence at `col..end` of `raw`, classified `k`.
fn code_kind_at(k: HitKind, raw: &[u8], col: usize, end: usize) -> Option<HitKind> {
    if k != HitKind::Str {
        return Some(k);
    }
    if !interpolated(raw, col) {
        return None;
    }
    let rest = raw.get(end..).unwrap_or_default();
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
    // as the other verbs, --hidden scans
    let opened = if o.use_index && !o.hidden && !order.is_empty() {
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
        so.kinds = [use_kinds(d.kind, &d.change), &[HitKind::Str]].concat();
        so.mode = Mode::Content;
        so.budget = 0;
        so.use_index = use_index;
        so.fresh = greeg_index::fresh::Mode::None;
        let mut r = scan(&so)?;
        let idxs: Vec<usize> = (0..r.files.len()).collect();
        crate::refine(&mut r, &idxs);
        let (mut sites, mut values, mut weak) = (Vec::new(), Vec::new(), Vec::new());
        let (mut total, mut direct_total, mut values_total, mut edited, mut files) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        let wanted = use_kinds(d.kind, &d.change);
        // a value use whose occurrence declares a new local
        let binds = |h: &Hit, kind: HitKind| {
            let end = (h.match_end - h.line_start) as usize;
            is_value_use(d, kind) && binding(&h.raw, h.column() as usize, end)
        };
        for f in &mut r.files {
            if f.hits
                .iter()
                .any(|h| code_kind(h).is_some_and(|k| binds(h, k)))
            {
                f.source();
            }
            let f = &*f;
            let kinds = f
                .src
                .as_ref()
                .map(|s| ScanKinds::new(f.lang, &s.bytes, s.bytes.len()));
            let changed = changes.files.iter().find(|c| c.rel == f.rel);
            let mut counted = false;
            for h in &f.hits {
                let Some(mut kind) = code_kind(h).filter(|k| wanted.contains(k)) else {
                    continue;
                };
                // the import line already counts as an import
                if is_value_use(d, kind)
                    && f.hits
                        .iter()
                        .any(|o| o.line == h.line && o.kind == HitKind::Import)
                {
                    continue;
                }
                // a binding declares a new local; the line counts only through
                // another occurrence of the name
                if binds(h, kind) {
                    match past_binding(h, f.lang, kinds.as_ref(), wanted) {
                        Some(k) => kind = k,
                        None => continue,
                    }
                }
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
                    if is_value_use(d, kind) {
                        values_total += 1;
                        values.push(site);
                    } else {
                        sites.push(site);
                    }
                } else {
                    weak.push(site);
                }
            }
        }
        sites.append(&mut values);
        sites.append(&mut weak);
        sites.truncate(MAX_SITES);
        let (others, others_total) = other_definitions(&so, &name, &changes, fi, d);
        impacts.push(DefImpact {
            file: fi,
            def: di,
            sites,
            sites_total: total,
            direct_total,
            values_total,
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

    #[test]
    fn bindings_declare_a_local() {
        let at = |s: &str, w: &str| {
            let i = s.find(w).unwrap();
            binding(s.as_bytes(), i, i + w.len())
        };
        for s in [
            "    let mut cb = 0;",
            "let cb: u8 = 1;",
            "val cb = 1",
            "for cb in xs:",
            "with open(p) as cb:",
            "cb = 0",
            "a, cb = pair",
            "def g(xs, cb):",
            "pub(crate) fn g(cb: u8) {}",
            "async function g(cb) {",
            "private fun g(cb: Int) {",
            "fun on(f: (Int) -> Unit, cb: Int) {",
            "async function* g(cb) {",
            "export default function(cb) {",
            "fn g<F: Fn(u8) -> u8>(f: F, cb: u8) {}",
        ] {
            assert!(at(s, "cb"), "{s}");
        }
        for s in [
            "bus.subscribe(cb)",
            "xs = [cb]",
            "if cb == g:",
            "x => cb",
            "call(a, cb)",
            "def g(x=cb):",
            "&mut cb",
            "fn pair() -> (cb, u8) {",
            "fn on(f: impl Fn(cb) -> u8) {}",
            "fun on(f: (cb) -> Unit) {",
            "fun make() = wrap(cb)",
            "function setup() { bus.on(\"e\", cb); }",
            "functions.append(cb)",
            "functionMap.set(key, cb)",
            "let cb { x } = p;",
            "if let cb { x, .. } = p {",
            "let cb(x) = w;",
        ] {
            assert!(!at(s, "cb"), "{s}");
        }
    }
}
