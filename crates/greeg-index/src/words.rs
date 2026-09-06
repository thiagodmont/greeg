//! Word index (DESIGN.md §3.2, M10): one posting list per distinct identifier
//! token of a file, next to the trigram postings. A whole-word query (`-w
//! NAME`, a bare identifier in a ranked layout, `refs`, `\bNAME\b`) then
//! opens only the files that contain the word, where the trigram plan opened
//! every file sharing its trigrams (TypeScript-5.9 `createSourceFile`: 486
//! candidates for 30 files with a match).
//!
//! A word is a maximal run of ASCII word bytes (`[A-Za-z0-9_]`) of 2 to 64
//! bytes, case preserved. Bytes ≥ 0x80 separate words, so every whole-word
//! match of ripgrep's `\b` (ASCII or Unicode) on an ASCII word is such a run
//! and the candidate set is a superset; a query word with other bytes, or
//! under `-i`, keeps the trigram plan.

use anyhow::{Context, Result, bail};

pub const MIN_WORD: usize = 2;
pub const MAX_WORD: usize = 64;

#[inline]
pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Can this query word be answered from the index? (ASCII word bytes only,
/// within the indexed length range.)
pub fn is_query_word(w: &[u8]) -> bool {
    (MIN_WORD..=MAX_WORD).contains(&w.len()) && w.iter().all(|&b| is_word_byte(b))
}

/// Call `f` on every indexed word of `src`, in order, duplicates included.
pub fn for_each_word(src: &[u8], mut f: impl FnMut(&[u8])) {
    let n = src.len();
    let mut i = 0;
    while i < n {
        if !is_word_byte(src[i]) {
            i += 1;
            continue;
        }
        let s = i;
        i += 1;
        while i < n && is_word_byte(src[i]) {
            i += 1;
        }
        if (MIN_WORD..=MAX_WORD).contains(&(i - s)) {
            f(&src[s..i]);
        }
    }
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// Serialize a word section from (word, document count, serialized bitmap)
/// entries sorted by word:
///
/// ```text
/// u32 n_words, u32 arena_len, u64 postings_len
/// u32 word_off[n_words + 1]   into arena; words sorted bytewise
/// u8  arena                   padded to 8
/// u32 counts[n_words]         padded to 8
/// u64 offsets[n_words + 1]    into postings
/// postings                    roaring bitmap per word, padded to 8
/// ```
pub fn serialize(entries: &[(Vec<u8>, u32, Vec<u8>)]) -> Vec<u8> {
    let n = entries.len();
    let arena_len: usize = entries.iter().map(|(w, _, _)| w.len()).sum();
    let postings_len: u64 = entries.iter().map(|(_, _, b)| b.len() as u64).sum();
    let mut body = Vec::with_capacity(32 + n * 20 + arena_len + postings_len as usize);
    body.extend_from_slice(&(n as u32).to_le_bytes());
    body.extend_from_slice(&(arena_len as u32).to_le_bytes());
    body.extend_from_slice(&postings_len.to_le_bytes());
    let mut off = 0u32;
    for (w, _, _) in entries {
        body.extend_from_slice(&off.to_le_bytes());
        off += w.len() as u32;
    }
    body.extend_from_slice(&off.to_le_bytes());
    for (w, _, _) in entries {
        body.extend_from_slice(w);
    }
    pad8(&mut body);
    for (_, c, _) in entries {
        body.extend_from_slice(&c.to_le_bytes());
    }
    pad8(&mut body);
    let mut off = 0u64;
    for (_, _, b) in entries {
        body.extend_from_slice(&off.to_le_bytes());
        off += b.len() as u64;
    }
    body.extend_from_slice(&off.to_le_bytes());
    for (_, _, b) in entries {
        body.extend_from_slice(b);
    }
    pad8(&mut body);
    body
}

/// Zero-copy view over a serialized word section.
pub struct WordsView<'a> {
    pub word_off: &'a [u32],
    pub arena: &'a [u8],
    pub counts: &'a [u32],
    pub offsets: &'a [u64],
    pub postings: &'a [u8],
}

impl<'a> WordsView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short words section");
        }
        let n = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
        let plen = u64::from_le_bytes(body[8..16].try_into().unwrap()) as usize;
        let mut off = 16;
        let ob = body.get(off..off + (n + 1) * 4).context("word offsets")?;
        let word_off: &[u32] =
            bytemuck::try_cast_slice(ob).map_err(|_| anyhow::anyhow!("unaligned word offsets"))?;
        off += (n + 1) * 4;
        let arena = body.get(off..off + arena_len).context("word arena")?;
        off = (off + arena_len + 7) & !7;
        let cb = body.get(off..off + n * 4).context("word counts")?;
        let counts: &[u32] =
            bytemuck::try_cast_slice(cb).map_err(|_| anyhow::anyhow!("unaligned word counts"))?;
        off = (off + n * 4 + 7) & !7;
        let pb = body
            .get(off..off + (n + 1) * 8)
            .context("posting offsets")?;
        let offsets: &[u64] = bytemuck::try_cast_slice(pb)
            .map_err(|_| anyhow::anyhow!("unaligned posting offsets"))?;
        off += (n + 1) * 8;
        let postings = body.get(off..off + plen).context("word postings")?;
        if word_off.last().copied().unwrap_or(0) as usize != arena_len
            || offsets.last().copied().unwrap_or(0) as usize != plen
        {
            bail!("word section lengths disagree");
        }
        Ok(WordsView {
            word_off,
            arena,
            counts,
            offsets,
            postings,
        })
    }
    pub fn len(&self) -> usize {
        self.counts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
    pub fn word(&self, i: usize) -> &'a [u8] {
        &self.arena[self.word_off[i] as usize..self.word_off[i + 1] as usize]
    }
    /// Dictionary position of `w`, if indexed.
    pub fn find(&self, w: &[u8]) -> Option<usize> {
        let n = self.len();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.word(mid).cmp(w) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
    pub fn posting_bytes(&self, i: usize) -> &'a [u8] {
        &self.postings[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
    /// Dictionary position of the word whose bytes contain arena offset `off`.
    pub fn word_at_offset(&self, off: usize) -> Option<usize> {
        let i = self.word_off.partition_point(|&o| (o as usize) <= off);
        if i == 0 || i > self.len() {
            return None;
        }
        Some(i - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &[u8]) -> Vec<String> {
        let mut v = Vec::new();
        for_each_word(src, |w| v.push(String::from_utf8_lossy(w).into_owned()));
        v
    }

    #[test]
    fn tokenizer_boundaries() {
        assert_eq!(
            words(b"fn createSourceFile(x: u8) -> T2 { x_y.z; }"),
            ["fn", "createSourceFile", "u8", "T2", "x_y"]
        );
        // one-byte words are not indexed, bytes >= 0x80 separate words
        assert_eq!(
            words("a foo\u{e9}bar b\u{b7}c z".as_bytes()),
            ["foo", "bar"]
        );
        let long = "x".repeat(65);
        assert!(words(long.as_bytes()).is_empty());
        assert_eq!(words("y".repeat(64).as_bytes()).len(), 1);
        assert!(is_query_word(b"node"));
        assert!(!is_query_word(b"a"));
        assert!(!is_query_word("caf\u{e9}".as_bytes()));
        assert!(!is_query_word(b"get_queryset("));
    }

    #[test]
    fn roundtrip() {
        let mut entries: Vec<(Vec<u8>, u32, Vec<u8>)> = Vec::new();
        for (w, ids) in [
            ("Node", vec![1u32, 5]),
            ("alpha", vec![2]),
            ("node", vec![1, 2, 3]),
        ] {
            let bm: roaring::RoaringBitmap = ids.iter().copied().collect();
            let mut b = Vec::new();
            bm.serialize_into(&mut b).unwrap();
            entries.push((w.as_bytes().to_vec(), ids.len() as u32, b));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let body = serialize(&entries);
        let v = WordsView::parse(&body).unwrap();
        assert_eq!(v.len(), 3);
        let i = v.find(b"node").unwrap();
        assert_eq!(v.word(i), b"node");
        assert_eq!(v.counts[i], 3);
        let bm = roaring::RoaringBitmap::deserialize_from(v.posting_bytes(i)).unwrap();
        assert_eq!(bm.iter().collect::<Vec<_>>(), [1, 2, 3]);
        assert!(v.find(b"NODE").is_none(), "case-sensitive keys");
        assert_eq!(v.word_at_offset(v.word_off[i] as usize + 2), Some(i));
        assert_eq!(v.find(b"Node").map(|i| v.counts[i]), Some(2));
    }
}
