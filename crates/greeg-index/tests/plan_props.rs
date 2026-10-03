//! The planner never loses a match: for seeded random patterns and lines, a
//! line the pattern matches holds the grams its plan asks for (no false
//! negatives), case-insensitive and literal patterns included.
//! `GREEG_TEST_SEED` runs another seed.

use greeg_index::gram::{Dedup, fold_buf};
use greeg_index::plan::{Q, plan};
use regex::bytes::RegexBuilder;
use std::collections::HashSet;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

const ALPHABET: &[&str] = &[
    "a", "b", "f", "o", "x", "A", "B", "F", "O", "X", "_", "1", "2", " ", "\t", ".", "(", "é", "É",
    "ß", "日", "-",
];

/// A pattern and a way to produce strings it matches.
enum Node {
    Lit(String),
    Class(&'static str, &'static [&'static str]),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, &'static str, usize, usize),
    Bound,
}

fn lit(rng: &mut Rng) -> String {
    (0..1 + rng.below(5))
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

fn node(rng: &mut Rng, depth: usize) -> Node {
    let leaf = depth >= 3 || rng.chance(3);
    if leaf {
        return match rng.below(8) {
            0 => Node::Class("[abc]", &["a", "b", "c"]),
            1 => Node::Class("[a-f]", &["a", "d", "f"]),
            2 => Node::Class(r"\d", &["0", "7"]),
            3 => Node::Class(r"\w", &["a", "Z", "_", "9", "é"]),
            4 => Node::Class(".", &["x", " ", "é"]),
            5 if rng.chance(2) => Node::Bound,
            _ => Node::Lit(lit(rng)),
        };
    }
    match rng.below(3) {
        0 => Node::Concat(
            (0..2 + rng.below(3))
                .map(|_| node(rng, depth + 1))
                .collect(),
        ),
        1 => Node::Alt(
            (0..2 + rng.below(2))
                .map(|_| node(rng, depth + 1))
                .collect(),
        ),
        _ => {
            let (op, lo, hi) =
                [("*", 0, 3), ("+", 1, 3), ("?", 0, 1), ("{2,3}", 2, 3)][rng.below(4)];
            Node::Repeat(Box::new(node(rng, depth + 1)), op, lo, hi)
        }
    }
}

fn render(n: &Node) -> String {
    match n {
        Node::Lit(s) => regex_syntax::escape(s),
        Node::Class(c, _) => c.to_string(),
        Node::Concat(v) => v.iter().map(render).collect(),
        Node::Alt(v) => format!("(?:{})", v.iter().map(render).collect::<Vec<_>>().join("|")),
        Node::Repeat(b, op, _, _) => format!("(?:{}){op}", render(b)),
        Node::Bound => r"\b".into(),
    }
}

/// A string `n` matches (word boundaries may still fail in context).
fn sample(n: &Node, rng: &mut Rng, casei: bool) -> String {
    match n {
        Node::Lit(s) if casei => s
            .chars()
            .map(|c| {
                if rng.chance(2) {
                    c.to_uppercase().collect::<String>()
                } else {
                    c.to_lowercase().collect()
                }
            })
            .collect(),
        Node::Lit(s) => s.clone(),
        Node::Class(_, members) => members[rng.below(members.len())].to_string(),
        Node::Concat(v) => v.iter().map(|c| sample(c, rng, casei)).collect(),
        Node::Alt(v) => sample(&v[rng.below(v.len())], rng, casei),
        Node::Repeat(b, _, lo, hi) => (0..lo + rng.below(hi - lo + 1))
            .map(|_| sample(b, rng, casei))
            .collect(),
        Node::Bound => String::new(),
    }
}

fn satisfied(q: &Q, grams: &HashSet<u32>) -> bool {
    match q {
        Q::All => true,
        Q::None => false,
        Q::Gram(k) => grams.contains(k),
        Q::And(v) => v.iter().all(|q| satisfied(q, grams)),
        Q::Or(v) => v.iter().any(|q| satisfied(q, grams)),
    }
}

fn grams(line: &[u8], dedup: &mut Dedup) -> HashSet<u32> {
    let mut buf = line.to_vec();
    fold_buf(&mut buf);
    let mut out = Vec::new();
    dedup.extract(&buf, &mut out);
    out.into_iter().collect()
}

#[test]
fn a_matching_line_holds_the_grams_its_plan_asks_for() {
    let seed = std::env::var("GREEG_TEST_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(0x9e37_79b9_7f4a_7c15, |s| s | 1);
    let mut rng = Rng(seed);
    let mut dedup = Dedup::new();
    let (mut matched, mut narrowed) = (0, 0);
    for _ in 0..3000 {
        let casei = rng.chance(3);
        let fixed = rng.chance(4);
        let n = if fixed {
            Node::Lit(lit(&mut rng))
        } else {
            node(&mut rng, 0)
        };
        let pattern = match &n {
            Node::Lit(s) if fixed => s.clone(),
            _ => render(&n),
        };
        let q = plan(&pattern, fixed, casei).unwrap();
        if q != Q::All {
            narrowed += 1;
        }
        let re = RegexBuilder::new(&if fixed {
            regex_syntax::escape(&pattern)
        } else {
            pattern.clone()
        })
        .case_insensitive(casei)
        .build()
        .unwrap();
        for _ in 0..8 {
            let line = format!(
                "{}{}{}",
                lit(&mut rng),
                sample(&n, &mut rng, casei),
                lit(&mut rng)
            );
            if re.is_match(line.as_bytes()) {
                matched += 1;
                assert!(
                    satisfied(&q, &grams(line.as_bytes(), &mut dedup)),
                    "seed {seed}: pattern {pattern:?} (fixed {fixed}, -i {casei}) matches {line:?}, plan {q:?} rejects it"
                );
            }
        }
    }
    eprintln!("{matched} matching lines, {narrowed} narrowed plans");
    assert!(matched > 15_000, "{matched} matching lines");
    assert!(narrowed > 800, "{narrowed} plans narrower than all files");
}
