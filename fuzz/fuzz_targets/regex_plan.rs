//! The planner never loses a match: a text the regex matches holds the grams
//! its plan asks for. Input: a flags byte (1 literal, 2 case-insensitive),
//! then the pattern and the text, split at the first NUL.
#![no_main]

use greeg_index::gram::{Dedup, fold_buf};
use greeg_index::plan::{Q, plan};
use libfuzzer_sys::fuzz_target;
use std::collections::HashSet;

fn satisfied(q: &Q, grams: &HashSet<u32>) -> bool {
    match q {
        Q::All => true,
        Q::None => false,
        Q::Gram(k) => grams.contains(k),
        Q::And(v) => v.iter().all(|q| satisfied(q, grams)),
        Q::Or(v) => v.iter().any(|q| satisfied(q, grams)),
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let (fixed, casei) = (flags & 1 != 0, flags & 2 != 0);
    let (pattern, text) = match rest.iter().position(|&b| b == 0) {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[][..]),
    };
    let Ok(pattern) = std::str::from_utf8(pattern) else {
        return;
    };
    let Ok(q) = plan(pattern, fixed, casei) else {
        return;
    };
    let source = if fixed {
        regex_syntax::escape(pattern)
    } else {
        pattern.to_string()
    };
    let Ok(re) = regex::bytes::RegexBuilder::new(&source)
        .case_insensitive(casei)
        .size_limit(1 << 20)
        .build()
    else {
        return;
    };
    if re.is_match(text) {
        let mut folded = text.to_vec();
        fold_buf(&mut folded);
        let mut grams = Vec::new();
        Dedup::new().extract(&folded, &mut grams);
        let grams: HashSet<u32> = grams.into_iter().collect();
        assert!(
            satisfied(&q, &grams),
            "{pattern:?} matches {text:?}, but its plan {q:?} does not"
        );
    }
});
