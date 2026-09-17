//! On-disk layouts. All integers little-endian. Every file starts with a
//! 16-byte header: magic "GREEG", format u16, component u8, payload length
//! u64. Fixed-width tables are 8-byte aligned so they can be
//! viewed as `&[u32]`/`&[u64]` straight from the mmap.
//!
//! files.<gen>.bin
//!   u32 n_files, u32 n_dirs, u32 arena_len, u32 pad
//!   FileRec[n_files]   (32 bytes each)
//!   DirRec[n_dirs]     (16 bytes each)
//!   arena bytes (paths, relative, '/'-separated), padded to 8
//!   u32 n_huge, u32 n_hidden, u32 huge[n_huge], u32 hidden[n_hidden]   (segment-local ids, padded to 8)
//!
//! grams.<gen>.bin
//!   u32 n_grams, u32 pad, u64 postings_len
//!   u32 keys[n_grams]        sorted
//!   u32 counts[n_grams]      documents per gram
//!   u64 offsets[n_grams + 1] into postings
//!   postings bytes           roaring portable serialization per gram
//!
//! symbols.<gen>.bin (ARCHITECTURE.md)
//!   u32 n_files, u32 n_syms, u32 n_names, u32 n_super, u32 n_tokens, u32 n_tok_names, u32 arena_len, u32 tok_arena_len
//!   u32 sym_off[n_files + 1]        CSR: symbols of file f are syms[sym_off[f]..sym_off[f+1]]
//!   SymRec[n_syms]                  (40 bytes each, file order, start order within a file)
//!   u32 supers[n_super]             name ids referenced by SymRec.super_off/len
//!   u32 name_off[n_names + 1]       into the name arena; names are sorted bytewise
//!   u8  arena[arena_len]
//!   u32 by_name_off[n_names + 1]    into by_name
//!   u32 by_name[n_syms]             symbol ids grouped by name id, best first
//!   u32 tok_off[n_tokens + 1]       split tokens (lowercase, sorted)
//!   u8  tok_arena[tok_arena_len]
//!   u32 tok_names_off[n_tokens + 1]
//!   u32 tok_names[n_tok_names]      name ids per token
//!   every array is padded to 8 bytes
//!
//! spans.<gen>.bin (§3.4)
//!   u32 n_files, u32 n_nc, u32 n_imp, u32 arena_len
//!   u32 nc_off[n_files + 1]; NcRec[n_nc] (8 bytes: start, end | kind << 30)
//!   u32 imp_off[n_files + 1]; ImpRec[n_imp] (20 bytes)
//!   u8 arena (raw module strings)
//!
//! graph.<gen>.bin (§3.5)
//!   u32 n_files, u32 n_edges, u32 pad, u32 pad
//!   u32 out_off[n_files + 1]; u32 out_to[n_edges]; u16 out_w[n_edges]
//!   u32 in_off[n_files + 1];  u32 in_from[n_edges]
//!   f32 rank[n_files]
//!
//! words.<gen>.bin: word dictionary + postings (`words.rs`)
//!
//! delta/NNNN.bin: u32 first_id, n_files, files_len, grams_len, symbols_len,
//! spans_len, graph_len, words_len (32 bytes), then the sections in that order
//! (same layouts; FileRec ids are absolute: first_id + local offset), plus a
//! tombstone bitmap of superseded ids at the end.
//!
//! Writers (build publish, delta apply) hold an exclusive `flock` on `LOCK`;
//! readers never lock (ARCHITECTURE.md).

use anyhow::{Context, Result, bail};
use bytemuck::{Pod, Zeroable};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

pub const MAGIC: &[u8; 5] = b"GREEG";
pub const HEADER_LEN: usize = 16;
pub const COMP_FILES: u8 = 1;
pub const COMP_GRAMS: u8 = 2;
pub const COMP_DELTA: u8 = 3;
pub const COMP_TOMB: u8 = 4;
pub const COMP_SYMBOLS: u8 = 5;
pub const COMP_SPANS: u8 = 6;
pub const COMP_GRAPH: u8 = 7;
pub const COMP_WORDS: u8 = 8;
/// Delta segment header: eight u32 (first_id, n_files, six section lengths).
pub const DELTA_HEADER: usize = 32;

pub const NONE: u32 = u32::MAX;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct FileRec {
    pub path_off: u32,
    pub path_len: u16,
    pub lang: u8,
    pub flags8: u8,
    pub size: u64,
    pub mtime_ns: i64,
    pub dir: u32,
    pub flags: u16,
    /// Quantized PageRank (0 = unknown, else 1..=65535 linear in rank_norm).
    pub rank: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct DirRec {
    pub path_off: u32,
    pub path_len: u16,
    pub pad: u16,
    pub mtime_ns: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default)]
pub struct SymRec {
    pub name_id: u32,
    pub file: u32,
    pub start: u32,
    pub end: u32,
    pub name_start: u32,
    pub line: u32,
    /// Symbol id of the enclosing definition (segment-local) or NONE.
    pub parent: u32,
    pub super_off: u32,
    pub kind: u8,
    pub flags: u8,
    pub name_len: u16,
    pub super_len: u8,
    pub pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct NcRec {
    pub start: u32,
    /// end | kind << 30 (0 comment, 1 string, 2 docstring)
    pub end_kind: u32,
}

impl NcRec {
    pub fn end(&self) -> u32 {
        self.end_kind & 0x3fff_ffff
    }
    pub fn kind(&self) -> u8 {
        (self.end_kind >> 30) as u8
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ImpRec {
    pub start: u32,
    pub end: u32,
    /// Resolved file id or NONE.
    pub target: u32,
    pub raw_off: u32,
    pub raw_len: u16,
    /// bit 0: wildcard; bits 1..: number of imported names (saturating)
    pub info: u16,
}

/// 16 bytes: "GREEG" (5), format u16 (2), component u8 (1), payload length u64 (8).
pub fn write_header(w: &mut impl Write, comp: u8, payload_len: u64) -> Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&crate::FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&[comp])?;
    w.write_all(&payload_len.to_le_bytes())?;
    Ok(())
}

pub fn check_header(bytes: &[u8], comp: u8) -> Result<&[u8]> {
    if bytes.len() < HEADER_LEN || &bytes[..5] != MAGIC {
        bail!("not a greeg index file");
    }
    let fmt = u16::from_le_bytes([bytes[5], bytes[6]]);
    if fmt != crate::FORMAT_VERSION {
        bail!("index format {fmt} != {}", crate::FORMAT_VERSION);
    }
    if bytes[7] != comp {
        bail!("wrong component {} (want {comp})", bytes[7]);
    }
    let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    if bytes.len() < HEADER_LEN + len {
        bail!("truncated index file");
    }
    Ok(&bytes[HEADER_LEN..HEADER_LEN + len])
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

/// Ignore files are tracked for freshness (an edit re-evaluates the ignore
/// rules) but, like every hidden file, never searched.
pub fn is_ignore_file(rel: &str) -> bool {
    let name = rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel);
    matches!(name, ".gitignore" | ".ignore" | ".rgignore")
}

/// In-memory file table used while building and when writing deltas.
#[derive(Clone, Debug, Default)]
pub struct FileTable {
    pub files: Vec<FileRec>,
    pub dirs: Vec<DirRec>,
    pub arena: Vec<u8>,
    /// Segment-local ids of files above `MAX_FILE`: no grams, always a candidate.
    pub huge: Vec<u32>,
    /// Segment-local ids of tracked-only files (ignore files): never a candidate.
    pub hidden: Vec<u32>,
}

impl FileTable {
    pub fn intern(&mut self, s: &str) -> (u32, u16) {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(s.as_bytes());
        (off, s.len().min(u16::MAX as usize) as u16)
    }
    /// Append a file record, classifying it into the huge/hidden lists.
    pub fn push_file(&mut self, rel: &str, rec: FileRec) {
        let id = self.files.len() as u32;
        if greeg_lang::FileFlags(rec.flags).has(greeg_lang::FileFlags::HUGE) {
            self.huge.push(id);
        }
        if is_ignore_file(rel) {
            self.hidden.push(id);
        }
        self.files.push(rec);
    }
    pub fn serialize(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(
            self.files.len() * 32
                + self.dirs.len() * 16
                + self.arena.len()
                + 48
                + (self.huge.len() + self.hidden.len()) * 4,
        );
        body.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        body.extend_from_slice(&(self.dirs.len() as u32).to_le_bytes());
        body.extend_from_slice(&(self.arena.len() as u32).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(bytemuck::cast_slice(&self.files));
        body.extend_from_slice(bytemuck::cast_slice(&self.dirs));
        body.extend_from_slice(&self.arena);
        pad8(&mut body);
        body.extend_from_slice(&(self.huge.len() as u32).to_le_bytes());
        body.extend_from_slice(&(self.hidden.len() as u32).to_le_bytes());
        body.extend_from_slice(bytemuck::cast_slice(&self.huge));
        body.extend_from_slice(bytemuck::cast_slice(&self.hidden));
        pad8(&mut body);
        body
    }
}

/// Zero-copy view over a serialized file table.
pub struct FilesView<'a> {
    pub files: &'a [FileRec],
    pub dirs: &'a [DirRec],
    pub arena: &'a [u8],
    pub huge: &'a [u32],
    pub hidden: &'a [u32],
}

impl<'a> FilesView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short files section");
        }
        let n_files = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let n_dirs = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
        let arena_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        let mut off = 16;
        let fb = body
            .get(off..off + n_files * 32)
            .context("files table truncated")?;
        off += n_files * 32;
        let db = body
            .get(off..off + n_dirs * 16)
            .context("dirs table truncated")?;
        off += n_dirs * 16;
        let arena = body.get(off..off + arena_len).context("arena truncated")?;
        off = (off + arena_len + 7) & !7;
        let counts = body.get(off..off + 8).context("id lists truncated")?;
        let n_huge = u32::from_le_bytes(counts[0..4].try_into().unwrap()) as usize;
        let n_hidden = u32::from_le_bytes(counts[4..8].try_into().unwrap()) as usize;
        off += 8;
        let hb = body
            .get(off..off + n_huge * 4)
            .context("huge list truncated")?;
        off += n_huge * 4;
        let ib = body
            .get(off..off + n_hidden * 4)
            .context("hidden list truncated")?;
        let huge: &[u32] =
            bytemuck::try_cast_slice(hb).map_err(|_| anyhow::anyhow!("unaligned huge list"))?;
        let hidden: &[u32] =
            bytemuck::try_cast_slice(ib).map_err(|_| anyhow::anyhow!("unaligned hidden list"))?;
        if huge.iter().chain(hidden).any(|&i| i as usize >= n_files) {
            bail!("id list out of range");
        }
        Ok(FilesView {
            files: bytemuck::cast_slice(fb),
            dirs: bytemuck::cast_slice(db),
            arena,
            huge,
            hidden,
        })
    }
    pub fn path(&self, f: &FileRec) -> &'a str {
        std::str::from_utf8(
            &self.arena[f.path_off as usize..f.path_off as usize + f.path_len as usize],
        )
        .unwrap_or("")
    }
    pub fn dir_path(&self, d: &DirRec) -> &'a str {
        std::str::from_utf8(
            &self.arena[d.path_off as usize..d.path_off as usize + d.path_len as usize],
        )
        .unwrap_or("")
    }
}

/// Serialized gram section from sorted (key, serialized bitmap) pairs.
pub fn serialize_grams(entries: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
    let n = entries.len();
    let postings_len: u64 = entries.iter().map(|(_, _, b)| b.len() as u64).sum();
    let mut body = Vec::with_capacity(16 + n * 16 + 8 + postings_len as usize);
    body.extend_from_slice(&(n as u32).to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&postings_len.to_le_bytes());
    for (k, _, _) in entries {
        body.extend_from_slice(&k.to_le_bytes());
    }
    for (_, c, _) in entries {
        body.extend_from_slice(&c.to_le_bytes());
    }
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

pub struct GramsView<'a> {
    pub keys: &'a [u32],
    pub counts: &'a [u32],
    pub offsets: &'a [u64],
    pub postings: &'a [u8],
}

impl<'a> GramsView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short grams section");
        }
        let n = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let plen = u64::from_le_bytes(body[8..16].try_into().unwrap()) as usize;
        let mut off = 16;
        let keys: &[u32] = bytemuck::cast_slice(body.get(off..off + n * 4).context("keys")?);
        off += n * 4;
        let counts: &[u32] = bytemuck::cast_slice(body.get(off..off + n * 4).context("counts")?);
        off += n * 4;
        // offsets table starts at 16 + 8n; 8-byte aligned iff n is even — realign by copying is
        // avoided by reading with from_le_bytes when misaligned.
        let ob = body.get(off..off + (n + 1) * 8).context("offsets")?;
        off += (n + 1) * 8;
        let postings = body.get(off..off + plen).context("postings")?;
        let offsets: &[u64] = match bytemuck::try_cast_slice(ob) {
            Ok(s) => s,
            Err(_) => bail!("unaligned offsets table"),
        };
        Ok(GramsView {
            keys,
            counts,
            offsets,
            postings,
        })
    }
    pub fn find(&self, key: u32) -> Option<usize> {
        self.keys.binary_search(&key).ok()
    }
    pub fn posting_bytes(&self, i: usize) -> &'a [u8] {
        &self.postings[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
}

static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A temp name next to `path`, unique per process and call.
pub fn tmp_path(path: &Path) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()))
}

/// Write a component file atomically (unique temp + rename).
pub fn write_atomic(path: &Path, comp: u8, body: &[u8]) -> Result<()> {
    write_atomic_with(path, comp, body, true)
}

/// `write_atomic` with the fsync optional. Base components are published
/// durably (a build is seconds anyway); a delta segment skips it because
/// `F_FULLFSYNC` costs 4–5 ms of every post-edit query on APFS, and a delta
/// torn by a crash fails `Index::open` and triggers a rebuild (ARCHITECTURE.md).
pub fn write_atomic_with(path: &Path, comp: u8, body: &[u8], durable: bool) -> Result<()> {
    let tmp = tmp_path(path);
    {
        let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        write_header(&mut f, comp, body.len() as u64)?;
        f.write_all(body)?;
        f.flush()?;
        if durable {
            f.get_ref().sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
