//! Request selection: one query's positional paths, globs, types and
//! exclusion flags, compiled once and applied to indexed files the way the
//! walker applies them in a scan. Every index-backed answer (text search and
//! symbol verbs) admits files through it before counting or ranking.

use crate::Options;
use anyhow::Result;
use greeg_lang::FileFlags;
use ignore::overrides::Override;
use ignore::types::Types;

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
