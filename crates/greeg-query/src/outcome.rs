//! The outcome of a query, whatever renders it: how the pattern matched, how
//! much of the eligible answer is shown, where the evidence came from and
//! how fresh it is. The exit status derives from it alone (C14): 0 only when
//! the query as given found something; a relaxed or empty answer is 1.

use crate::{Rung, ScanResult};

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// Eligible results: matched lines for a search, entries for a verb.
    pub total: usize,
    /// Results the answer shows; fewer than `total` only under a budget.
    pub shown: usize,
    /// `Exact`, or the relaxed rung whose names answered.
    pub rung: Rung,
    /// "index", "index (phase 1)" or "scan".
    pub source: &'static str,
    /// The freshness check the index answer ran ("ttl", "stat", "fsevents",
    /// "none"); empty for a scan, which reads the files themselves.
    pub fresh: &'static str,
    /// Files changed since the index was published, read from disk instead.
    pub deferred: usize,
}

impl Outcome {
    /// The outcome of a search answer that shows `shown` of its lines.
    pub fn of_search(r: &ScanResult, shown: usize) -> Outcome {
        let index = r.stats.source != "scan";
        Outcome {
            total: r.stats.total_hits,
            shown: shown.min(r.stats.total_hits),
            rung: r.rung.clone(),
            source: r.stats.source,
            fresh: if index { r.stats.fresh_method } else { "" },
            deferred: r.stats.fresh_deferred,
        }
    }

    pub fn exact(&self) -> bool {
        self.rung == Rung::Exact
    }

    pub fn complete(&self) -> bool {
        self.shown >= self.total
    }

    pub fn exit_code(&self) -> i32 {
        if self.total > 0 && self.exact() { 0 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(total: usize, rung: Rung) -> Outcome {
        Outcome {
            total,
            shown: total,
            rung,
            source: "index",
            fresh: "stat",
            deferred: 0,
        }
    }

    #[test]
    fn only_an_exact_nonempty_answer_succeeds() {
        assert_eq!(outcome(3, Rung::Exact).exit_code(), 0);
        assert_eq!(outcome(0, Rung::Exact).exit_code(), 1);
        assert_eq!(outcome(3, Rung::CaseInsensitive).exit_code(), 1);
        assert_eq!(outcome(3, Rung::Fuzzy(vec!["x".into()])).exit_code(), 1);
    }
}
