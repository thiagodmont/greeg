//! Regex → trigram query (ARCHITECTURE.md), after Russ Cox's analysis.
//!
//! Each HIR node yields (emptyable, exact set, prefix set, suffix set, query).
//! Sets are bounded; when a set grows past its limit its trigrams are folded
//! into the query and the set degrades to "unknown".

use crate::gram::literal_keys;
use anyhow::Result;
use regex_syntax::hir::{Class, Hir, HirKind, Look};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Q {
    All,
    None,
    Gram(u32),
    And(Vec<Q>),
    Or(Vec<Q>),
}

const MAX_SET: usize = 32;
const MAX_STR: usize = 24;

type Set = BTreeSet<Vec<u8>>;

#[derive(Clone, Debug)]
struct Info {
    emptyable: bool,
    exact: Option<Set>,
    prefix: Set,
    suffix: Set,
    q: Q,
}

fn one(s: &[u8]) -> Set {
    let mut v = BTreeSet::new();
    v.insert(s.to_vec());
    v
}

fn empty_str() -> Set {
    one(b"")
}

/// AND of trigram groups of every string in the set (OR across strings).
fn trigrams_of(set: &Set) -> Q {
    let mut alts = Vec::new();
    for s in set {
        let groups = literal_keys(s);
        if groups.is_empty() {
            return Q::All; // some string too short to constrain
        }
        let mut ands: Vec<Q> = groups
            .into_iter()
            .flat_map(|g| g.into_iter().map(Q::Gram))
            .collect();
        ands.dedup();
        alts.push(if ands.len() == 1 {
            ands.pop().unwrap()
        } else {
            Q::And(ands)
        });
    }
    if alts.is_empty() {
        Q::All
    } else if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Q::Or(alts)
    }
}

fn and(a: Q, b: Q) -> Q {
    match (a, b) {
        (Q::All, x) | (x, Q::All) => x,
        (Q::None, _) | (_, Q::None) => Q::None,
        (Q::And(mut x), Q::And(mut y)) => {
            x.append(&mut y);
            Q::And(x)
        }
        (Q::And(mut x), y) | (y, Q::And(mut x)) => {
            x.push(y);
            Q::And(x)
        }
        (x, y) => Q::And(vec![x, y]),
    }
}

fn or(a: Q, b: Q) -> Q {
    match (a, b) {
        (Q::All, _) | (_, Q::All) => Q::All,
        (Q::None, x) | (x, Q::None) => x,
        (Q::Or(mut x), Q::Or(mut y)) => {
            x.append(&mut y);
            Q::Or(x)
        }
        (Q::Or(mut x), y) | (y, Q::Or(mut x)) => {
            x.push(y);
            Q::Or(x)
        }
        (x, y) => Q::Or(vec![x, y]),
    }
}

fn cross(a: &Set, b: &Set) -> Set {
    let mut out = BTreeSet::new();
    for x in a {
        for y in b {
            let mut s = x.clone();
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    out
}

fn trim_prefixes(set: &Set) -> Set {
    set.iter()
        .map(|s| s[..s.len().min(MAX_STR)].to_vec())
        .collect()
}
fn trim_suffixes(set: &Set) -> Set {
    set.iter()
        .map(|s| s[s.len().saturating_sub(MAX_STR)..].to_vec())
        .collect()
}

/// Fold an oversized exact set into the query (Cox's simplification).
fn simplify(mut i: Info) -> Info {
    if let Some(ex) = &i.exact
        && (ex.len() > MAX_SET || ex.iter().any(|s| s.len() > MAX_STR))
    {
        i.q = and(i.q.clone(), trigrams_of(ex));
        i.prefix = trim_prefixes(ex)
            .into_iter()
            .map(|s| s[..s.len().min(3)].to_vec())
            .collect();
        i.suffix = trim_suffixes(ex)
            .into_iter()
            .map(|s| s[s.len().saturating_sub(3)..].to_vec())
            .collect();
        i.exact = None;
    }
    if i.prefix.len() > MAX_SET {
        i.prefix = i
            .prefix
            .iter()
            .map(|s| s[..s.len().min(2)].to_vec())
            .collect();
        if i.prefix.len() > MAX_SET {
            i.prefix = empty_str();
        }
    }
    if i.suffix.len() > MAX_SET {
        i.suffix = i
            .suffix
            .iter()
            .map(|s| s[s.len().saturating_sub(2)..].to_vec())
            .collect();
        if i.suffix.len() > MAX_SET {
            i.suffix = empty_str();
        }
    }
    i
}

fn analyze(h: &Hir) -> Info {
    let info = match h.kind() {
        HirKind::Empty | HirKind::Look(_) => Info {
            emptyable: true,
            exact: Some(empty_str()),
            prefix: empty_str(),
            suffix: empty_str(),
            q: Q::All,
        },
        HirKind::Literal(l) => {
            let b: Vec<u8> = l.0.iter().map(|&c| crate::gram::fold(c)).collect();
            Info {
                emptyable: b.is_empty(),
                exact: Some(one(&b)),
                prefix: one(&b),
                suffix: one(&b),
                q: Q::All,
            }
        }
        HirKind::Class(c) => {
            let members = class_members(c);
            match members {
                Some(ms) if !ms.is_empty() && ms.len() <= 8 => {
                    let set: Set = ms.into_iter().collect();
                    Info {
                        emptyable: false,
                        exact: Some(set.clone()),
                        prefix: set.clone(),
                        suffix: set,
                        q: Q::All,
                    }
                }
                _ => Info {
                    emptyable: false,
                    exact: None,
                    prefix: empty_str(),
                    suffix: empty_str(),
                    q: Q::All,
                },
            }
        }
        HirKind::Repetition(r) => {
            let sub = analyze(&r.sub);
            if r.min == 0 {
                Info {
                    emptyable: true,
                    exact: None,
                    prefix: empty_str(),
                    suffix: empty_str(),
                    q: Q::All,
                }
            } else if r.min == 1 && r.max == Some(1) {
                sub
            } else {
                // e{n,m} with n>=1: at least one copy; prefix/suffix of e; query of e
                Info {
                    emptyable: sub.emptyable,
                    exact: None,
                    prefix: sub.prefix,
                    suffix: sub.suffix,
                    q: sub.q,
                }
            }
        }
        HirKind::Capture(c) => analyze(&c.sub),
        HirKind::Concat(parts) => {
            let mut acc = Info {
                emptyable: true,
                exact: Some(empty_str()),
                prefix: empty_str(),
                suffix: empty_str(),
                q: Q::All,
            };
            for p in parts {
                let b = analyze(p);
                acc = concat(acc, b);
            }
            acc
        }
        HirKind::Alternation(alts) => {
            let mut it = alts.iter();
            let mut acc = analyze(it.next().unwrap());
            for a in it {
                let b = analyze(a);
                acc = alternate(acc, b);
            }
            acc
        }
    };
    simplify(info)
}

fn concat(a: Info, b: Info) -> Info {
    let mut q = and(a.q.clone(), b.q.clone());
    let exact = match (&a.exact, &b.exact) {
        (Some(x), Some(y)) => Some(cross(x, y)),
        _ => None,
    };
    // trigrams spanning the boundary; implied by the exact set while one is
    // still tracked (every exact string contains one of the boundary strings),
    // and emitting them there would nest an OR per step for multi-member sets
    if exact.is_none() {
        let mid = cross(&a.suffix, &b.prefix);
        q = and(q, trigrams_of(&mid));
    }
    let prefix = match &a.exact {
        Some(x) => cross(x, &b.prefix),
        None => a.prefix.clone(),
    };
    let suffix = match &b.exact {
        Some(y) => cross(&a.suffix, y),
        None => b.suffix.clone(),
    };
    let mut out = Info {
        emptyable: a.emptyable && b.emptyable,
        exact,
        prefix,
        suffix,
        q,
    };
    if let Some(ex) = &out.exact
        && ex.len() > MAX_SET
    {
        out = simplify(out);
    }
    simplify(out)
}

fn alternate(a: Info, b: Info) -> Info {
    let exact = match (&a.exact, &b.exact) {
        (Some(x), Some(y)) => Some(x.union(y).cloned().collect()),
        _ => None,
    };
    let with_exact = |i: &Info| {
        and(
            i.q.clone(),
            i.exact.as_ref().map(trigrams_of).unwrap_or(Q::All),
        )
    };
    let q = match (&a.exact, &b.exact) {
        (Some(_), Some(_)) => Q::All, // handled through exact when finalized
        _ => or(with_exact(&a), with_exact(&b)),
    };
    simplify(Info {
        emptyable: a.emptyable || b.emptyable,
        exact,
        prefix: a.prefix.union(&b.prefix).cloned().collect(),
        suffix: a.suffix.union(&b.suffix).cloned().collect(),
        q,
    })
}

/// Byte strings matched by a class if it is small, else None. ASCII members
/// are case-folded; other code points are their UTF-8 bytes, which the index
/// stores raw. With `-i`, `s` and `k` become classes that also hold U+017F
/// (ſ) and U+212A (K): keeping those as members preserves the gram chain
/// through every `s`/`k` while a file spelt with `ſ` stays a candidate.
fn class_members(c: &Class) -> Option<Vec<Vec<u8>>> {
    let mut out = BTreeSet::new();
    match c {
        Class::Unicode(u) => {
            for r in u.iter() {
                let (s, e) = (r.start() as u32, r.end() as u32);
                if e - s > 16 {
                    return None;
                }
                for cp in s..=e {
                    match char::from_u32(cp) {
                        Some(ch) if ch.is_ascii() => out.insert(vec![crate::gram::fold(cp as u8)]),
                        Some(ch) => out.insert(ch.to_string().into_bytes()),
                        None => return None,
                    };
                }
            }
        }
        Class::Bytes(b) => {
            for r in b.iter() {
                let (s, e) = (r.start(), r.end());
                if e - s > 16 {
                    return None;
                }
                for x in s..=e {
                    if x >= 0x80 {
                        return None;
                    }
                    out.insert(vec![crate::gram::fold(x)]);
                }
            }
        }
    }
    Some(out.into_iter().collect())
}

/// Remove redundant structure; keep at most `k` grams per AND (the evaluator
/// sorts by document count so the rarest survive).
pub fn flatten(q: Q) -> Q {
    match q {
        Q::And(v) => {
            let mut out = Vec::new();
            for x in v {
                match flatten(x) {
                    Q::All => {}
                    Q::None => return Q::None,
                    Q::And(mut inner) => out.append(&mut inner),
                    other => out.push(other),
                }
            }
            out.sort_by_key(|x| match x {
                Q::Gram(g) => *g as u64,
                _ => u64::MAX,
            });
            out.dedup();
            match out.len() {
                0 => Q::All,
                1 => out.pop().unwrap(),
                _ => Q::And(out),
            }
        }
        Q::Or(v) => {
            let mut out = Vec::new();
            for x in v {
                match flatten(x) {
                    Q::All => return Q::All,
                    Q::None => {}
                    Q::Or(mut inner) => out.append(&mut inner),
                    other => out.push(other),
                }
            }
            match out.len() {
                0 => Q::None,
                1 => out.pop().unwrap(),
                _ => Q::Or(out),
            }
        }
        other => other,
    }
}

/// Plan a gram query for a pattern. `fixed` = literal string, `casei` = -i.
pub fn plan(pattern: &str, fixed: bool, casei: bool) -> Result<Q> {
    let pat = if fixed {
        regex_syntax::escape(pattern)
    } else {
        pattern.to_string()
    };
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(casei)
        .build()
        .parse(&pat)?;
    let info = analyze(&hir);
    let mut q = info.q;
    if let Some(ex) = &info.exact {
        q = and(q, trigrams_of(ex));
    }
    Ok(flatten(q))
}

/// Whole-word plan (ARCHITECTURE.md): the words a file must contain for
/// the pattern to match as a whole word, as alternatives, or `None` when the
/// trigram plan must answer. `identifier` is the bare-identifier answer mode
/// (the answer is the whole word); `word` is `-w`; a `\bWORD\b` regex, or an
/// alternation of such, qualifies on its own. Case-insensitive queries and
/// words the index does not hold (non-ASCII bytes, 1 or > 64 bytes) keep the
/// trigram plan.
pub fn word_plan(
    pattern: &str,
    fixed: bool,
    casei: bool,
    word: bool,
    identifier: bool,
) -> Option<Vec<Vec<u8>>> {
    use crate::words::is_query_word;
    if casei {
        return None;
    }
    if identifier && is_query_word(pattern.as_bytes()) {
        return Some(vec![pattern.as_bytes().to_vec()]);
    }
    if fixed {
        return (word && is_query_word(pattern.as_bytes()))
            .then(|| vec![pattern.as_bytes().to_vec()]);
    }
    // The pattern must denote a finite set of token sequences built from word
    // boundaries and literals (regex-syntax factors shared `\b`s out of an
    // alternation, so the shape is recovered from the sequences, not the tree).
    #[derive(Clone, PartialEq)]
    enum Tok {
        Bound,
        Lit(Vec<u8>),
    }
    const MAX_SEQS: usize = 64;
    fn seqs(h: &Hir) -> Option<Vec<Vec<Tok>>> {
        match h.kind() {
            HirKind::Empty => Some(vec![vec![]]),
            HirKind::Look(Look::WordAscii | Look::WordUnicode) => Some(vec![vec![Tok::Bound]]),
            HirKind::Literal(l) => Some(vec![vec![Tok::Lit(l.0.to_vec())]]),
            HirKind::Capture(c) => seqs(&c.sub),
            HirKind::Alternation(alts) => {
                let mut out = Vec::new();
                for a in alts {
                    out.extend(seqs(a)?);
                    if out.len() > MAX_SEQS {
                        return None;
                    }
                }
                Some(out)
            }
            HirKind::Concat(parts) => {
                let mut acc: Vec<Vec<Tok>> = vec![vec![]];
                for p in parts {
                    let ps = seqs(p)?;
                    let mut next = Vec::with_capacity(acc.len() * ps.len());
                    for a in &acc {
                        for s in &ps {
                            let mut v = a.clone();
                            v.extend(s.iter().cloned());
                            next.push(v);
                        }
                    }
                    if next.len() > MAX_SEQS {
                        return None;
                    }
                    acc = next;
                }
                Some(acc)
            }
            _ => None,
        }
    }
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(pattern)
        .ok()?;
    let mut out = Vec::new();
    for s in seqs(&hir)? {
        // `-w` supplies the boundaries; otherwise the pattern must carry them
        let lit = match s.as_slice() {
            [Tok::Lit(l)] if word => l,
            [Tok::Bound, Tok::Lit(l), Tok::Bound] => l,
            _ => return None,
        };
        if !is_query_word(lit) {
            return None;
        }
        if !out.contains(lit) {
            out.push(lit.clone());
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grams(q: &Q) -> usize {
        match q {
            Q::Gram(_) => 1,
            Q::And(v) | Q::Or(v) => v.iter().map(grams).sum(),
            _ => 0,
        }
    }
    #[test]
    fn literal() {
        let q = plan("createSourceFile", false, false).unwrap();
        assert_eq!(grams(&q), 14);
    }
    #[test]
    fn word_plans() {
        let w = |s: &str| s.as_bytes().to_vec();
        assert_eq!(
            word_plan("node", false, false, true, false),
            Some(vec![w("node")])
        );
        assert_eq!(
            word_plan("node", true, false, true, false),
            Some(vec![w("node")])
        );
        assert_eq!(
            word_plan("node", false, false, false, true),
            Some(vec![w("node")])
        );
        assert_eq!(
            word_plan("node", false, false, false, false),
            None,
            "substring semantics"
        );
        assert_eq!(
            word_plan("node", false, true, true, false),
            None,
            "-i keeps the grams"
        );
        assert_eq!(
            word_plan("foo|bar_baz", false, false, true, false),
            Some(vec![w("foo"), w("bar_baz")])
        );
        assert_eq!(
            word_plan("(?:foo)|(?:bar)", false, false, true, false),
            Some(vec![w("foo"), w("bar")])
        );
        assert_eq!(
            word_plan(r"\bfoo\b", false, false, false, false),
            Some(vec![w("foo")])
        );
        assert_eq!(
            word_plan(r"\b(foo|bar)\b", false, false, false, false),
            Some(vec![w("foo"), w("bar")])
        );
        assert_eq!(
            word_plan(r"(?:\bfoo\b)|(?:\bbar\b)", false, false, false, false),
            Some(vec![w("foo"), w("bar")])
        );
        assert_eq!(word_plan(r"\bfoo", false, false, false, false), None);
        assert_eq!(word_plan(r"get_\w+", false, false, true, false), None);
        assert_eq!(word_plan("foo bar", false, false, true, false), None);
        assert_eq!(word_plan("caf\u{e9}", false, false, true, false), None);
        assert_eq!(word_plan("x", false, false, true, false), None, "one byte");
        assert_eq!(
            word_plan("a|bc", false, false, true, false),
            None,
            "one alternative too short"
        );
    }
    #[test]
    fn fixed_and_short() {
        assert_eq!(plan("ab", true, false).unwrap(), Q::All);
        assert!(matches!(plan("a.b", true, false).unwrap(), Q::Gram(_)));
    }
    #[test]
    fn alternation_and_classes() {
        let q = plan("(foo|bar)baz", false, false).unwrap();
        assert!(matches!(q, Q::Or(_)) || matches!(q, Q::And(_)), "{q:?}");
        let q = plan("get_[a-z]+set", false, false).unwrap();
        assert_eq!(grams(&q), 3, "{q:?}"); // get, et_, set
        assert_eq!(plan(r"\w{5}\s+\w{5}", false, false).unwrap(), Q::All);
        let q = plan("def get_queryset", false, true).unwrap();
        assert!(grams(&q) >= 10);
    }
    #[test]
    fn case_insensitive_keeps_grams_through_s_and_k() {
        // -i turns `s`/`k` into classes holding U+017F/U+212A; the chain must
        // survive as an OR of the ASCII spelling and the folded spellings
        let q = plan("getUserSession", false, true).unwrap();
        assert!(matches!(q, Q::Or(_)), "{q:?}");
        let Q::Or(alts) = &q else { unreachable!() };
        assert_eq!(alts.len(), 16, "{q:?}"); // four `s`, each ∈ {s, ſ}
        assert!(
            alts.contains(&Q::And(
                literal_keys(b"getusersession")
                    .remove(0)
                    .into_iter()
                    .map(Q::Gram)
                    .collect()
            )),
            "{q:?}"
        );
        assert!(
            alts.contains(&Q::And(
                literal_keys("getuſerſeſſion".as_bytes())
                    .remove(0)
                    .into_iter()
                    .map(Q::Gram)
                    .collect()
            )),
            "{q:?}"
        );
        let q = plan("createSourceFile", false, true).unwrap();
        assert_eq!(grams(&q), 14 + 15, "{q:?}"); // `createſourcefile` is a byte longer
        let q = plan("kilo", true, true).unwrap();
        let Q::Or(alts) = &q else { panic!("{q:?}") };
        assert!(
            alts.contains(&Q::And(
                literal_keys("\u{212A}ilo".as_bytes())
                    .remove(0)
                    .into_iter()
                    .map(Q::Gram)
                    .collect()
            )),
            "{q:?}"
        );
        // without -i nothing changes
        assert!(matches!(
            plan("getUserSession", false, false).unwrap(),
            Q::And(_)
        ));
    }
}
