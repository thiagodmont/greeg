//! The outcome of a query, whatever renders it: how the pattern matched, how
//! much of the eligible answer is shown, where the evidence came from and
//! how fresh it is. The exit status derives from it alone (C14): 0 only when
//! the query as given found something; a relaxed or empty answer is 1.

use crate::{FileResult, Rung, ScanResult};
use greeg_lang::Lang;
use std::collections::BTreeSet;

/// Matching files that hold code but have no grammar (`Lang::None`): the
/// definition rules and call classification never looked inside them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoGrammar {
    pub files: usize,
    /// Extensions, sorted, lowercase and without the dot.
    pub languages: Vec<String>,
}

impl NoGrammar {
    /// The no-grammar files among `files`, if any.
    pub fn of(files: &[FileResult]) -> Option<NoGrammar> {
        Self::of_paths(
            files
                .iter()
                .filter(|f| f.lang == Lang::None)
                .map(|f| f.rel.as_slice()),
        )
    }

    /// Root-relative paths already known to have no grammar; only those whose
    /// extension names a programming language count.
    pub fn of_paths<'a>(rels: impl Iterator<Item = &'a [u8]>) -> Option<NoGrammar> {
        let mut files = 0;
        let mut languages = BTreeSet::new();
        for rel in rels {
            let Some(ext) = greeg_index::rel::as_path(rel).extension() else {
                continue;
            };
            let ext = ext.to_string_lossy();
            if greeg_lang::is_code_without_grammar(&ext) {
                files += 1;
                languages.insert(ext.to_lowercase());
            }
        }
        (files > 0).then(|| NoGrammar {
            files,
            languages: languages.into_iter().collect(),
        })
    }

    /// `1 matching file has no grammar (go)`.
    pub fn describe(&self) -> String {
        let (noun, verb) = if self.files == 1 {
            ("file", "has")
        } else {
            ("files", "have")
        };
        format!(
            "{} matching {noun} {verb} no grammar ({})",
            self.files,
            self.languages.join(", ")
        )
    }
}

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
    /// `--max-bytes`, not the budget, left results out.
    pub byte_cut: bool,
    /// Matching files the verb could not classify; none when there are none.
    pub no_grammar: Option<NoGrammar>,
    /// Files over `--max-filesize` that were not read.
    pub skipped_huge: usize,
    /// A fixed limit, not the budget, left names out of the answer; it
    /// leaves `complete()` alone and shows as `truncated_by: "cap"`.
    pub capped: Option<&'static str>,
}

impl Outcome {
    /// The outcome of a shaped search answer.
    pub fn of_answer(r: &ScanResult, ft: &crate::shape::Footer) -> Outcome {
        Outcome {
            byte_cut: ft.byte_cut,
            ..Self::of_search(r, ft.hits_shown)
        }
    }

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
            byte_cut: false,
            no_grammar: None,
            skipped_huge: r.stats.skipped_huge,
            capped: None,
        }
    }

    /// The same outcome, noting files without a grammar.
    pub fn with_no_grammar(self, no_grammar: Option<NoGrammar>) -> Outcome {
        Outcome { no_grammar, ..self }
    }

    /// The same outcome, counting the files `--max-filesize` skipped.
    pub fn with_skipped_huge(self, skipped_huge: usize) -> Outcome {
        Outcome {
            skipped_huge,
            ..self
        }
    }

    pub fn exact(&self) -> bool {
        self.rung == Rung::Exact
    }

    pub fn complete(&self) -> bool {
        self.shown >= self.total
    }

    /// What left results out: `bytes` or `budget` when the answer is
    /// incomplete, else `cap` when a fixed limit left names out.
    pub fn truncated_by(&self) -> Option<&'static str> {
        if self.complete() {
            self.capped.map(|_| "cap")
        } else {
            Some(if self.byte_cut { "bytes" } else { "budget" })
        }
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
            byte_cut: false,
            no_grammar: None,
            skipped_huge: 0,
            capped: None,
        }
    }

    #[test]
    fn only_an_exact_nonempty_answer_succeeds() {
        assert_eq!(outcome(3, Rung::Exact).exit_code(), 0);
        assert_eq!(outcome(0, Rung::Exact).exit_code(), 1);
        assert_eq!(outcome(3, Rung::CaseInsensitive).exit_code(), 1);
        assert_eq!(outcome(3, Rung::Fuzzy(vec!["x".into()])).exit_code(), 1);
    }

    #[test]
    fn no_grammar_names_files_and_extensions() {
        let ng = NoGrammar::of_paths([&b"a/x.go"[..], b"B.java", b"y.GO"].into_iter()).unwrap();
        assert_eq!(ng.files, 3);
        assert_eq!(ng.languages, ["go", "java"]);
        assert_eq!(ng.describe(), "3 matching files have no grammar (go, java)");
        assert_eq!(NoGrammar::of_paths(std::iter::empty()), None);
    }

    #[test]
    fn no_grammar_counts_only_code_extensions() {
        let text = [
            &b"Makefile"[..],
            b"LICENSE",
            b".gitignore",
            b"NOTES.MD",
            b"build.log",
            b"guide.adoc",
        ];
        assert_eq!(NoGrammar::of_paths(text.into_iter()), None);
        let ng = NoGrammar::of_paths(text.into_iter().chain([&b"go/x.go"[..]])).unwrap();
        assert_eq!(ng.files, 1);
        assert_eq!(ng.languages, ["go"]);
    }

    #[test]
    fn a_fixed_limit_is_a_cap_that_leaves_the_counts_complete() {
        let capped = Outcome {
            capped: Some("called by: first 12 callers, 12 names each"),
            ..outcome(3, Rung::Exact)
        };
        assert!(capped.complete());
        assert_eq!(capped.truncated_by(), Some("cap"));
        assert_eq!(outcome(3, Rung::Exact).truncated_by(), None);
    }

    #[test]
    fn of_search_copies_the_skipped_huge_count() {
        let r = ScanResult {
            opts: crate::Options::default(),
            files: Vec::new(),
            stats: crate::Stats {
                skipped_huge: 2,
                ..Default::default()
            },
            rung: Rung::Exact,
            ignored_only: None,
            ignored_partial: false,
            related_index: Vec::new(),
        };
        assert_eq!(Outcome::of_search(&r, 0).skipped_huge, 2);
        assert_eq!(
            Outcome::of_search(&r, 0).with_skipped_huge(5).skipped_huge,
            5
        );
    }
}
