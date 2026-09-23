//! Request selection: one query's positional paths, globs, types and
//! exclusion flags, compiled once and applied to indexed files the way the
//! walker applies them in a scan. Every index-backed answer (text search and
//! symbol verbs) admits files through it before counting or ranking.

use crate::Options;
use anyhow::Result;
use greeg_index::Index;
use greeg_index::skipped::{DIR, IGNORED, UNKNOWN};
use greeg_lang::FileFlags;
use ignore::overrides::Override;
use ignore::types::Types;

/// What a request reaches among the entries the index skipped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    No,
    /// A file to read from disk alongside the indexed candidates.
    File,
    /// A directory the walk would enter: only a scan covers it.
    Dir,
}

pub(crate) struct Selection<'a> {
    o: &'a Options,
    /// Root-relative positional paths; empty selects the whole root.
    paths: Vec<String>,
    overrides: Option<Override>,
    types: Option<Types>,
}

impl<'a> Selection<'a> {
    pub(crate) fn new(o: &'a Options, paths: Vec<String>) -> Result<Self> {
        let overrides = if o.globs.is_empty() {
            None
        } else {
            let mut ob = ignore::overrides::OverrideBuilder::new(&o.root);
            for g in &o.globs {
                ob.add(g)?;
            }
            Some(ob.build()?)
        };
        let types = if o.types.is_empty() && o.types_not.is_empty() {
            None
        } else {
            Some(crate::build_types(o)?)
        };
        Ok(Selection {
            o,
            paths,
            overrides,
            types,
        })
    }

    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Can this request select files the index leaves out? `--hidden` and
    /// `--no-ignore` always can; a positive glob can select hidden and
    /// ignored entries, and a type hidden files.
    pub(crate) fn widens(&self) -> bool {
        self.o.hidden
            || self.o.no_ignore
            || !self.o.types.is_empty()
            || self
                .overrides
                .as_ref()
                .is_some_and(|ov| ov.num_whitelists() > 0)
    }

    /// Does the request reach a skipped entry (`greeg_index::skipped`)?
    /// ripgrep checks an override glob before ignore rules and hidden names,
    /// and a type after ignore rules but before hidden names.
    pub(crate) fn reaches(&self, rel: &str, bits: u8) -> Reach {
        if bits & UNKNOWN != 0 {
            // children not listed exactly: whatever the request selects there
            let inside = |p: &String| {
                rel.is_empty()
                    || p.is_empty()
                    || p.starts_with(rel) && p.as_bytes().get(rel.len()) == Some(&b'/')
            };
            let overlaps =
                crate::indexed::path_allowed(rel, &self.paths) || self.paths.iter().any(inside);
            return if overlaps { Reach::Dir } else { Reach::No };
        }
        if bits & DIR != 0 {
            let walked = crate::indexed::path_allowed(rel, &self.paths)
                && self
                    .overrides
                    .as_ref()
                    .is_some_and(|ov| ov.matched(rel, true).is_whitelist());
            return if walked { Reach::Dir } else { Reach::No };
        }
        let by_glob = match self.overrides.as_ref().map(|ov| ov.matched(rel, false)) {
            Some(m) if m.is_ignore() => return Reach::No,
            Some(m) => m.is_whitelist(),
            None => false,
        };
        let by_type = bits & IGNORED == 0
            && self
                .types
                .as_ref()
                .is_some_and(|t| t.matched(rel, false).is_whitelist());
        if (by_glob || by_type) && self.selects(rel, greeg_lang::path_flags(rel)) {
            Reach::File
        } else {
            Reach::No
        }
    }

    /// The skipped files this request selects besides the indexed ones, or
    /// `None` when only a scan covers it: `--hidden`, `--no-ignore`, a
    /// skipped directory it would enter, or an index without the record (a
    /// background build then records it). `pending` holds changes found but
    /// not yet published.
    pub(crate) fn coverage(
        &self,
        idx: &Index,
        pending: Option<&greeg_index::fresh::Changes>,
    ) -> Option<Vec<String>> {
        if self.o.hidden || self.o.no_ignore {
            return None;
        }
        if !self.widens() {
            return Some(Vec::new());
        }
        let Some(mut sk) = idx.skipped() else {
            crate::indexed::spawn_build(&self.o.root, &idx.dir);
            return None;
        };
        if let Some(ch) = pending {
            sk.update(&ch.skipped);
        }
        let mut also = Vec::new();
        for (rel, bits) in sk.entries() {
            match self.reaches(&rel, bits) {
                Reach::No => {}
                Reach::File => also.push(rel),
                Reach::Dir => return None,
            }
        }
        Some(also)
    }

    /// Whether the request selects `rel`: ripgrep precedence, where an
    /// override glob decides first (a whitelist wins over types) and ignore
    /// globs also apply to every ancestor directory, as the walker would
    /// prune them; then the exclusion flags.
    pub(crate) fn selects(&self, rel: &str, flags: FileFlags) -> bool {
        if !crate::indexed::path_allowed(rel, &self.paths) {
            return false;
        }
        let mut decided = false;
        if let Some(ov) = &self.overrides {
            let m = ov.matched(rel, false);
            if m.is_ignore() {
                return false;
            }
            decided = m.is_whitelist();
            let mut end = 0;
            while let Some(k) = rel[end..].find('/') {
                end += k;
                if ov.matched(&rel[..end], true).is_ignore() {
                    return false;
                }
                end += 1;
            }
        }
        if !decided
            && let Some(t) = &self.types
            && t.matched(rel, false).is_ignore()
        {
            return false;
        }
        !crate::excluded_by_flags(self.o, flags)
    }

    /// A selected, searchable file's prior order: source 0, demoted 2,
    /// minified 3.
    pub(crate) fn admit(&self, rel: &str, flags: FileFlags) -> Option<u8> {
        if flags.has(FileFlags::BINARY) || !self.selects(rel, flags) {
            return None;
        }
        Some(if flags.has(FileFlags::MINIFIED) {
            3
        } else if flags.demoted() {
            2
        } else {
            0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlisted_directory_needs_the_scan_where_the_request_reaches_it() {
        let o = Options {
            globs: vec!["*.rs".into()],
            ..Default::default()
        };
        let whole = Selection::new(&o, Vec::new()).unwrap();
        assert_eq!(whole.reaches("src", UNKNOWN), Reach::Dir);
        let docs = Selection::new(&o, vec!["docs".into()]).unwrap();
        assert_eq!(docs.reaches("src", UNKNOWN), Reach::No);
        assert_eq!(docs.reaches("", UNKNOWN), Reach::Dir);
        assert_eq!(docs.reaches("docs/a", UNKNOWN), Reach::Dir);
    }
}
