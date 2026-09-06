//! Symbol, span and graph tables (DESIGN.md §3.3–§3.5): in-memory builders
//! used by the full build and by delta segments, and zero-copy views over the
//! serialized bytes. Layouts are documented in `format.rs`.

use crate::format::{ImpRec, NONE, NcRec, SymRec};
use anyhow::{Context, Result, bail};
use greeg_lang::DefKind;
use greeg_lang::lexer::SpanKind;
use greeg_lang::sym::{Extract, Import};
use hashbrown::HashMap;

pub const KIND_NAMES: [&str; 16] = [
    "fn",
    "method",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "mod",
    "object",
    "impl",
    "const",
    "var",
    "macro",
    "field",
    "variant",
];

pub fn kind_code(k: DefKind) -> u8 {
    match k {
        DefKind::Function => 0,
        DefKind::Method => 1,
        DefKind::Class => 2,
        DefKind::Struct => 3,
        DefKind::Enum => 4,
        DefKind::Trait => 5,
        DefKind::Interface => 6,
        DefKind::TypeAlias => 7,
        DefKind::Module => 8,
        DefKind::Object => 9,
        DefKind::Impl => 10,
        DefKind::Constant => 11,
        DefKind::Variable => 12,
        DefKind::Macro => 13,
        DefKind::Field => 14,
        DefKind::Variant => 15,
    }
}

pub fn kind_from_code(c: u8) -> DefKind {
    match c {
        0 => DefKind::Function,
        1 => DefKind::Method,
        2 => DefKind::Class,
        3 => DefKind::Struct,
        4 => DefKind::Enum,
        5 => DefKind::Trait,
        6 => DefKind::Interface,
        7 => DefKind::TypeAlias,
        8 => DefKind::Module,
        9 => DefKind::Object,
        10 => DefKind::Impl,
        11 => DefKind::Constant,
        12 => DefKind::Variable,
        13 => DefKind::Macro,
        14 => DefKind::Field,
        _ => DefKind::Variant,
    }
}

/// Definition-kind weight used to order same-name symbols (best first).
pub fn kind_weight(c: u8) -> f32 {
    match kind_from_code(c) {
        DefKind::Class
        | DefKind::Struct
        | DefKind::Enum
        | DefKind::Trait
        | DefKind::Interface
        | DefKind::Object
        | DefKind::Module => 1.0,
        DefKind::Function => 0.95,
        DefKind::Method => 0.9,
        DefKind::TypeAlias | DefKind::Macro => 0.85,
        DefKind::Constant => 0.7,
        DefKind::Variable => 0.6,
        DefKind::Field | DefKind::Variant => 0.5,
        DefKind::Impl => 0.4,
    }
}

/// Split an identifier into lowercase tokens of length ≥ 3 (camel, snake, Pascal).
pub fn split_tokens(name: &str) -> Vec<String> {
    let b = name.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 3 && !out.iter().any(|t| t == cur) {
            out.push(cur.clone());
        }
        cur.clear();
    };
    for i in 0..b.len() {
        let c = b[i];
        if !c.is_ascii_alphanumeric() {
            flush(&mut cur, &mut out);
            continue;
        }
        if c.is_ascii_uppercase() && i > 0 {
            let prev = b[i - 1];
            let next_lower = b
                .get(i + 1)
                .map(|n| n.is_ascii_lowercase())
                .unwrap_or(false);
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                flush(&mut cur, &mut out);
            }
        }
        cur.push(c.to_ascii_lowercase() as char);
    }
    flush(&mut cur, &mut out);
    out
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

fn put_u32s(v: &mut Vec<u8>, xs: &[u32]) {
    v.extend_from_slice(bytemuck::cast_slice(xs));
    pad8(v);
}

fn take_u32s<'a>(b: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u32]> {
    let s = b.get(*off..*off + n * 4).context("u32 array truncated")?;
    *off += n * 4;
    *off = (*off + 7) & !7;
    bytemuck::try_cast_slice(s).map_err(|_| anyhow::anyhow!("unaligned u32 array"))
}

fn take_bytes<'a>(b: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8]> {
    let s = b.get(*off..*off + n).context("byte array truncated")?;
    *off += n;
    *off = (*off + 7) & !7;
    Ok(s)
}

// ---------------------------------------------------------------- symbols

/// Per-file extraction result carried from the parallel phase to the merge.
#[derive(Debug, Default)]
pub struct FileExtract {
    pub symbols: Vec<greeg_lang::sym::Symbol>,
    /// Name per symbol.
    pub names: Vec<String>,
    /// Supertype names per symbol.
    pub supers: Vec<Vec<String>>,
    pub noncode: Vec<greeg_lang::lexer::Span>,
    pub imports: Vec<Import>,
    pub package: Option<String>,
    pub parse_errors: bool,
    pub tree_sitter: bool,
}

impl FileExtract {
    pub fn from_extract(ex: Extract, src: &[u8]) -> FileExtract {
        let names = ex
            .symbols
            .iter()
            .map(|s| ex.name(s, src).to_string())
            .collect();
        let supers = ex
            .symbols
            .iter()
            .map(|s| {
                s.supers
                    .iter()
                    .map(|&(a, b)| {
                        String::from_utf8_lossy(&src[a as usize..b as usize]).into_owned()
                    })
                    .collect()
            })
            .collect();
        FileExtract {
            symbols: ex.symbols,
            names,
            supers,
            noncode: ex.noncode,
            imports: ex.imports,
            package: ex.package,
            parse_errors: ex.parse_errors,
            tree_sitter: ex.tree_sitter,
        }
    }
    /// Exported top-level symbol names (for Kotlin import resolution).
    pub fn top_level_names(&self) -> impl Iterator<Item = &str> {
        self.symbols
            .iter()
            .zip(&self.names)
            .filter(|(s, _)| s.parent.is_none())
            .map(|(_, n)| n.as_str())
    }
}

#[derive(Default)]
pub struct SymBuilder {
    interner: HashMap<String, u32>,
    names: Vec<String>,
    sym_off: Vec<u32>,
    syms: Vec<SymRec>,
    supers: Vec<u32>,
    n_files: u32,
}

impl SymBuilder {
    pub fn new(n_files: u32) -> Self {
        SymBuilder {
            sym_off: Vec::with_capacity(n_files as usize + 1),
            n_files,
            ..Default::default()
        }
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.interner.get(s) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(s.to_string());
        self.interner.insert(s.to_string(), id);
        id
    }
    /// Files must be added in ascending local id order; gaps are filled with empty slices.
    pub fn add_file(&mut self, file_local: u32, ex: Option<&FileExtract>) {
        while self.sym_off.len() < file_local as usize + 1 {
            self.sym_off.push(self.syms.len() as u32);
        }
        let Some(ex) = ex else { return };
        let base = self.syms.len() as u32;
        for (i, s) in ex.symbols.iter().enumerate() {
            let name_id = self.intern(&ex.names[i]);
            let super_off = self.supers.len() as u32;
            for sp in &ex.supers[i] {
                let id = self.intern(sp);
                self.supers.push(id);
            }
            let super_len = ex.supers[i].len().min(255) as u8;
            self.syms.push(SymRec {
                name_id,
                file: file_local,
                start: s.start,
                end: s.end,
                name_start: s.name_start,
                line: s.line,
                parent: s.parent.map(|p| base + p).unwrap_or(NONE),
                super_off,
                kind: kind_code(s.kind),
                flags: s.flags,
                name_len: (s.name_end - s.name_start).min(u16::MAX as u32) as u16,
                super_len,
                pad: [0; 3],
            });
        }
    }
    pub fn n_symbols(&self) -> usize {
        self.syms.len()
    }

    /// Serialize. `rank_of(file_local)` orders same-name symbols; it may return 0.
    pub fn finish(mut self, rank_of: &dyn Fn(u32) -> f32) -> Vec<u8> {
        while self.sym_off.len() < self.n_files as usize + 1 {
            self.sym_off.push(self.syms.len() as u32);
        }
        // sorted name ids
        let mut order: Vec<u32> = (0..self.names.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            self.names[a as usize]
                .as_bytes()
                .cmp(self.names[b as usize].as_bytes())
        });
        let mut remap = vec![0u32; self.names.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u32;
        }
        for s in &mut self.syms {
            s.name_id = remap[s.name_id as usize];
        }
        for s in &mut self.supers {
            *s = remap[*s as usize];
        }
        let names: Vec<&str> = order
            .iter()
            .map(|&o| self.names[o as usize].as_str())
            .collect();
        // by_name: group symbol ids by name id, best first
        let mut groups: Vec<Vec<u32>> = vec![Vec::new(); names.len()];
        for (i, s) in self.syms.iter().enumerate() {
            groups[s.name_id as usize].push(i as u32);
        }
        let score = |s: &SymRec| -> f32 {
            let exported = if s.flags & greeg_lang::sym::SYM_EXPORTED != 0 {
                1.0
            } else {
                0.8
            };
            let test = if s.flags & greeg_lang::sym::SYM_TEST != 0 {
                0.5
            } else {
                1.0
            };
            kind_weight(s.kind) * exported * test * (0.6 + 0.4 * rank_of(s.file))
        };
        let mut by_name_off: Vec<u32> = Vec::with_capacity(names.len() + 1);
        let mut by_name: Vec<u32> = Vec::with_capacity(self.syms.len());
        for g in groups.iter_mut() {
            by_name_off.push(by_name.len() as u32);
            if g.len() > 1 {
                g.sort_by(|&a, &b| {
                    score(&self.syms[b as usize])
                        .partial_cmp(&score(&self.syms[a as usize]))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(self.syms[a as usize].file.cmp(&self.syms[b as usize].file))
                });
            }
            by_name.extend_from_slice(g);
        }
        by_name_off.push(by_name.len() as u32);
        // tokens
        let mut tok_map: HashMap<String, Vec<u32>> = HashMap::new();
        for (id, n) in names.iter().enumerate() {
            for t in split_tokens(n) {
                if t.as_str() != *n {
                    tok_map.entry(t).or_default().push(id as u32);
                }
            }
        }
        let mut toks: Vec<(String, Vec<u32>)> = tok_map.into_iter().collect();
        toks.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        // serialize
        let arena_len: usize = names.iter().map(|n| n.len()).sum();
        let tok_arena_len: usize = toks.iter().map(|(t, _)| t.len()).sum();
        let n_tok_names: usize = toks.iter().map(|(_, v)| v.len()).sum();
        let mut body =
            Vec::with_capacity(64 + self.syms.len() * 48 + arena_len * 2 + n_tok_names * 4);
        for x in [
            self.n_files,
            self.syms.len() as u32,
            names.len() as u32,
            self.supers.len() as u32,
            toks.len() as u32,
            n_tok_names as u32,
            arena_len as u32,
            tok_arena_len as u32,
        ] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        put_u32s(&mut body, &self.sym_off);
        body.extend_from_slice(bytemuck::cast_slice(&self.syms));
        pad8(&mut body);
        put_u32s(&mut body, &self.supers);
        let mut off = 0u32;
        let mut name_off = Vec::with_capacity(names.len() + 1);
        for n in &names {
            name_off.push(off);
            off += n.len() as u32;
        }
        name_off.push(off);
        put_u32s(&mut body, &name_off);
        for n in &names {
            body.extend_from_slice(n.as_bytes());
        }
        pad8(&mut body);
        put_u32s(&mut body, &by_name_off);
        put_u32s(&mut body, &by_name);
        let mut off = 0u32;
        let mut tok_off = Vec::with_capacity(toks.len() + 1);
        for (t, _) in &toks {
            tok_off.push(off);
            off += t.len() as u32;
        }
        tok_off.push(off);
        put_u32s(&mut body, &tok_off);
        for (t, _) in &toks {
            body.extend_from_slice(t.as_bytes());
        }
        pad8(&mut body);
        let mut off = 0u32;
        let mut tn_off = Vec::with_capacity(toks.len() + 1);
        let mut tn: Vec<u32> = Vec::with_capacity(n_tok_names);
        for (_, v) in &toks {
            tn_off.push(off);
            off += v.len() as u32;
            tn.extend_from_slice(v);
        }
        tn_off.push(off);
        put_u32s(&mut body, &tn_off);
        put_u32s(&mut body, &tn);
        body
    }
}

pub struct SymbolsView<'a> {
    pub n_files: u32,
    pub sym_off: &'a [u32],
    pub syms: &'a [SymRec],
    pub supers: &'a [u32],
    pub name_off: &'a [u32],
    pub arena: &'a [u8],
    pub by_name_off: &'a [u32],
    pub by_name: &'a [u32],
    pub tok_off: &'a [u32],
    pub tok_arena: &'a [u8],
    pub tok_names_off: &'a [u32],
    pub tok_names: &'a [u32],
}

impl<'a> SymbolsView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 32 {
            bail!("short symbols section");
        }
        let h: &[u32] = bytemuck::cast_slice(&body[..32]);
        let (n_files, n_syms, n_names, n_super, n_tokens, n_tok_names, arena_len, tok_arena_len) = (
            h[0] as usize,
            h[1] as usize,
            h[2] as usize,
            h[3] as usize,
            h[4] as usize,
            h[5] as usize,
            h[6] as usize,
            h[7] as usize,
        );
        let mut off = 32;
        let sym_off = take_u32s(body, &mut off, n_files + 1)?;
        let sb = body
            .get(off..off + n_syms * std::mem::size_of::<SymRec>())
            .context("syms truncated")?;
        off += sb.len();
        off = (off + 7) & !7;
        let syms: &[SymRec] =
            bytemuck::try_cast_slice(sb).map_err(|_| anyhow::anyhow!("unaligned syms"))?;
        let supers = take_u32s(body, &mut off, n_super)?;
        let name_off = take_u32s(body, &mut off, n_names + 1)?;
        let arena = take_bytes(body, &mut off, arena_len)?;
        let by_name_off = take_u32s(body, &mut off, n_names + 1)?;
        let by_name = take_u32s(body, &mut off, n_syms)?;
        let tok_off = take_u32s(body, &mut off, n_tokens + 1)?;
        let tok_arena = take_bytes(body, &mut off, tok_arena_len)?;
        let tok_names_off = take_u32s(body, &mut off, n_tokens + 1)?;
        let tok_names = take_u32s(body, &mut off, n_tok_names)?;
        Ok(SymbolsView {
            n_files: n_files as u32,
            sym_off,
            syms,
            supers,
            name_off,
            arena,
            by_name_off,
            by_name,
            tok_off,
            tok_arena,
            tok_names_off,
            tok_names,
        })
    }
    pub fn n_names(&self) -> usize {
        self.name_off.len().saturating_sub(1)
    }
    /// Symbols of a file (segment-local id) and the id of the first one.
    pub fn symbols_of(&self, file_local: u32) -> (u32, &'a [SymRec]) {
        let f = file_local as usize;
        if f + 1 >= self.sym_off.len() {
            return (0, &[]);
        }
        let (a, b) = (self.sym_off[f] as usize, self.sym_off[f + 1] as usize);
        (a as u32, &self.syms[a..b])
    }
    pub fn name(&self, name_id: u32) -> &'a str {
        let i = name_id as usize;
        if i + 1 >= self.name_off.len() {
            return "";
        }
        std::str::from_utf8(&self.arena[self.name_off[i] as usize..self.name_off[i + 1] as usize])
            .unwrap_or("")
    }
    pub fn find_name(&self, name: &str) -> Option<u32> {
        let n = self.n_names();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.name(mid as u32).as_bytes().cmp(name.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }
    /// Name ids whose name starts with `prefix`.
    pub fn prefix_range(&self, prefix: &str) -> std::ops::Range<u32> {
        let n = self.n_names();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.name(mid as u32).as_bytes() < prefix.as_bytes() {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        let mut hi = lo;
        while hi < n
            && self
                .name(hi as u32)
                .as_bytes()
                .starts_with(prefix.as_bytes())
        {
            hi += 1;
        }
        lo as u32..hi as u32
    }
    /// Symbol ids with this name, best first.
    pub fn syms_named(&self, name_id: u32) -> &'a [u32] {
        let i = name_id as usize;
        if i + 1 >= self.by_name_off.len() {
            return &[];
        }
        &self.by_name[self.by_name_off[i] as usize..self.by_name_off[i + 1] as usize]
    }
    pub fn supers_of(&self, s: &SymRec) -> &'a [u32] {
        let (a, b) = (
            s.super_off as usize,
            s.super_off as usize + s.super_len as usize,
        );
        self.supers.get(a..b).unwrap_or(&[])
    }
    pub fn token(&self, i: usize) -> &'a str {
        std::str::from_utf8(&self.tok_arena[self.tok_off[i] as usize..self.tok_off[i + 1] as usize])
            .unwrap_or("")
    }
    pub fn find_token(&self, tok: &str) -> Option<&'a [u32]> {
        let n = self.tok_off.len().saturating_sub(1);
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.token(mid).as_bytes().cmp(tok.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Some(
                        &self.tok_names[self.tok_names_off[mid] as usize
                            ..self.tok_names_off[mid + 1] as usize],
                    );
                }
            }
        }
        None
    }
    /// Innermost symbol containing `off` in file `file_local`; returns the symbol id.
    pub fn enclosing(&self, file_local: u32, off: u32) -> Option<u32> {
        let (base, syms) = self.symbols_of(file_local);
        let upto = syms.partition_point(|s| s.start <= off);
        (0..upto)
            .rev()
            .find(|&i| syms[i].end > off)
            .map(|i| base + i as u32)
    }
}

// ---------------------------------------------------------------- spans

#[derive(Default)]
pub struct SpanBuilder {
    n_files: u32,
    nc_off: Vec<u32>,
    nc: Vec<NcRec>,
    imp_off: Vec<u32>,
    imps: Vec<ImpRec>,
    arena: Vec<u8>,
}

impl SpanBuilder {
    pub fn new(n_files: u32) -> Self {
        SpanBuilder {
            n_files,
            ..Default::default()
        }
    }
    /// `targets[i]` is the resolved file id for `ex.imports[i]` (or NONE).
    pub fn add_file(&mut self, file_local: u32, ex: Option<&FileExtract>, targets: &[u32]) {
        while self.nc_off.len() < file_local as usize + 1 {
            self.nc_off.push(self.nc.len() as u32);
            self.imp_off.push(self.imps.len() as u32);
        }
        let Some(ex) = ex else { return };
        for sp in &ex.noncode {
            let k = match sp.kind {
                SpanKind::Comment => 0,
                SpanKind::String => 1,
                SpanKind::Docstring => 2,
            };
            self.nc.push(NcRec {
                start: sp.start,
                end_kind: (sp.end & 0x3fff_ffff) | (k << 30),
            });
        }
        for (i, im) in ex.imports.iter().enumerate() {
            let raw_off = self.arena.len() as u32;
            self.arena.extend_from_slice(im.module.as_bytes());
            let info = (im.wildcard as u16) | ((im.names.len().min(0x7fff) as u16) << 1);
            self.imps.push(ImpRec {
                start: im.start,
                end: im.end,
                target: targets.get(i).copied().unwrap_or(NONE),
                raw_off,
                raw_len: im.module.len().min(u16::MAX as usize) as u16,
                info,
            });
        }
    }
    pub fn finish(mut self) -> Vec<u8> {
        while self.nc_off.len() < self.n_files as usize + 1 {
            self.nc_off.push(self.nc.len() as u32);
            self.imp_off.push(self.imps.len() as u32);
        }
        let mut body =
            Vec::with_capacity(32 + self.nc.len() * 8 + self.imps.len() * 20 + self.arena.len());
        for x in [
            self.n_files,
            self.nc.len() as u32,
            self.imps.len() as u32,
            self.arena.len() as u32,
        ] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        put_u32s(&mut body, &self.nc_off);
        body.extend_from_slice(bytemuck::cast_slice(&self.nc));
        pad8(&mut body);
        put_u32s(&mut body, &self.imp_off);
        body.extend_from_slice(bytemuck::cast_slice(&self.imps));
        pad8(&mut body);
        body.extend_from_slice(&self.arena);
        pad8(&mut body);
        body
    }
}

pub struct SpansView<'a> {
    pub nc_off: &'a [u32],
    pub nc: &'a [NcRec],
    pub imp_off: &'a [u32],
    pub imps: &'a [ImpRec],
    pub arena: &'a [u8],
}

impl<'a> SpansView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short spans section");
        }
        let h: &[u32] = bytemuck::cast_slice(&body[..16]);
        let (n_files, n_nc, n_imp, arena_len) =
            (h[0] as usize, h[1] as usize, h[2] as usize, h[3] as usize);
        let mut off = 16;
        let nc_off = take_u32s(body, &mut off, n_files + 1)?;
        let b = body.get(off..off + n_nc * 8).context("nc truncated")?;
        off = (off + b.len() + 7) & !7;
        let nc: &[NcRec] =
            bytemuck::try_cast_slice(b).map_err(|_| anyhow::anyhow!("unaligned nc"))?;
        let imp_off = take_u32s(body, &mut off, n_files + 1)?;
        let b = body.get(off..off + n_imp * 20).context("imps truncated")?;
        off = (off + b.len() + 7) & !7;
        let imps: &[ImpRec] =
            bytemuck::try_cast_slice(b).map_err(|_| anyhow::anyhow!("unaligned imps"))?;
        let arena = take_bytes(body, &mut off, arena_len)?;
        Ok(SpansView {
            nc_off,
            nc,
            imp_off,
            imps,
            arena,
        })
    }
    pub fn noncode_of(&self, file_local: u32) -> &'a [NcRec] {
        let f = file_local as usize;
        if f + 1 >= self.nc_off.len() {
            return &[];
        }
        &self.nc[self.nc_off[f] as usize..self.nc_off[f + 1] as usize]
    }
    pub fn imports_of(&self, file_local: u32) -> &'a [ImpRec] {
        let f = file_local as usize;
        if f + 1 >= self.imp_off.len() {
            return &[];
        }
        &self.imps[self.imp_off[f] as usize..self.imp_off[f + 1] as usize]
    }
    pub fn raw(&self, i: &ImpRec) -> &'a str {
        std::str::from_utf8(
            &self.arena[i.raw_off as usize..i.raw_off as usize + i.raw_len as usize],
        )
        .unwrap_or("")
    }
    /// Noncode span containing `off`, if any: (kind code, start, end).
    pub fn noncode_at(&self, file_local: u32, off: u32) -> Option<(u8, u32, u32)> {
        let nc = self.noncode_of(file_local);
        let i = nc.partition_point(|n| n.start <= off);
        if i == 0 {
            return None;
        }
        let n = &nc[i - 1];
        if n.end() > off {
            Some((n.kind(), n.start, n.end()))
        } else {
            None
        }
    }
    pub fn import_at(&self, file_local: u32, off: u32) -> Option<&'a ImpRec> {
        let im = self.imports_of(file_local);
        let i = im.partition_point(|n| n.start <= off);
        if i == 0 {
            return None;
        }
        let n = &im[i - 1];
        if n.end > off { Some(n) } else { None }
    }
}

// ---------------------------------------------------------------- graph

pub struct GraphBuilder {
    n: u32,
    edges: Vec<(u32, u32, u16)>,
}

impl GraphBuilder {
    pub fn new(n: u32) -> Self {
        GraphBuilder {
            n,
            edges: Vec::new(),
        }
    }
    pub fn add(&mut self, from: u32, to: u32, w: u16) {
        if from != to && to != NONE {
            self.edges.push((from, to, w));
        }
    }
    pub fn n_edges(&self) -> usize {
        self.edges.len()
    }
    /// Build CSRs, run PageRank (damping 0.85, 20 iterations). Returns the
    /// serialized body and the rank vector (normalized to max = 1).
    pub fn finish(mut self) -> (Vec<u8>, Vec<f32>) {
        let n = self.n as usize;
        self.edges.sort_unstable();
        self.edges.dedup_by(|b, a| {
            if a.0 == b.0 && a.1 == b.1 {
                a.2 = a.2.saturating_add(b.2);
                true
            } else {
                false
            }
        });
        let m = self.edges.len();
        let mut out_off = vec![0u32; n + 1];
        for &(f, _, _) in &self.edges {
            out_off[f as usize + 1] += 1;
        }
        for i in 0..n {
            out_off[i + 1] += out_off[i];
        }
        let out_to: Vec<u32> = self.edges.iter().map(|e| e.1).collect();
        let out_w: Vec<u16> = self.edges.iter().map(|e| e.2).collect();
        let mut in_off = vec![0u32; n + 1];
        for &(_, t, _) in &self.edges {
            in_off[t as usize + 1] += 1;
        }
        for i in 0..n {
            in_off[i + 1] += in_off[i];
        }
        let mut in_from = vec![0u32; m];
        let mut fill = in_off.clone();
        for &(f, t, _) in &self.edges {
            in_from[fill[t as usize] as usize] = f;
            fill[t as usize] += 1;
        }
        // PageRank
        let mut rank = vec![1.0f32 / n.max(1) as f32; n];
        let mut next = vec![0f32; n];
        let d = 0.85f32;
        let out_sum: Vec<f32> = (0..n)
            .map(|i| {
                (out_off[i]..out_off[i + 1])
                    .map(|e| out_w[e as usize] as f32)
                    .sum()
            })
            .collect();
        for _ in 0..20 {
            let mut dangling = 0f32;
            for i in 0..n {
                if out_off[i] == out_off[i + 1] {
                    dangling += rank[i];
                }
            }
            let base = (1.0 - d) / n.max(1) as f32 + d * dangling / n.max(1) as f32;
            next.iter_mut().for_each(|x| *x = base);
            for i in 0..n {
                let (a, b) = (out_off[i] as usize, out_off[i + 1] as usize);
                if a == b {
                    continue;
                }
                let share = d * rank[i] / out_sum[i].max(1e-9);
                for e in a..b {
                    next[out_to[e] as usize] += share * out_w[e] as f32;
                }
            }
            std::mem::swap(&mut rank, &mut next);
        }
        let max = rank.iter().cloned().fold(0f32, f32::max).max(1e-12);
        for r in &mut rank {
            *r /= max;
        }
        let mut body = Vec::with_capacity(32 + m * 10 + n * 12);
        for x in [self.n, m as u32, 0, 0] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        put_u32s(&mut body, &out_off);
        put_u32s(&mut body, &out_to);
        body.extend_from_slice(bytemuck::cast_slice(&out_w));
        pad8(&mut body);
        put_u32s(&mut body, &in_off);
        put_u32s(&mut body, &in_from);
        body.extend_from_slice(bytemuck::cast_slice(&rank));
        pad8(&mut body);
        (body, rank)
    }
}

pub struct GraphView<'a> {
    pub n: u32,
    pub out_off: &'a [u32],
    pub out_to: &'a [u32],
    pub out_w: &'a [u16],
    pub in_off: &'a [u32],
    pub in_from: &'a [u32],
    pub rank: &'a [f32],
}

impl<'a> GraphView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short graph section");
        }
        let h: &[u32] = bytemuck::cast_slice(&body[..16]);
        let (n, m) = (h[0] as usize, h[1] as usize);
        let mut off = 16;
        let out_off = take_u32s(body, &mut off, n + 1)?;
        let out_to = take_u32s(body, &mut off, m)?;
        let wb = take_bytes(body, &mut off, m * 2)?;
        let out_w: &[u16] =
            bytemuck::try_cast_slice(wb).map_err(|_| anyhow::anyhow!("unaligned weights"))?;
        let in_off = take_u32s(body, &mut off, n + 1)?;
        let in_from = take_u32s(body, &mut off, m)?;
        let rb = take_bytes(body, &mut off, n * 4)?;
        let rank: &[f32] =
            bytemuck::try_cast_slice(rb).map_err(|_| anyhow::anyhow!("unaligned ranks"))?;
        Ok(GraphView {
            n: n as u32,
            out_off,
            out_to,
            out_w,
            in_off,
            in_from,
            rank,
        })
    }
    pub fn out(&self, f: u32) -> &'a [u32] {
        let i = f as usize;
        if i + 1 >= self.out_off.len() {
            return &[];
        }
        &self.out_to[self.out_off[i] as usize..self.out_off[i + 1] as usize]
    }
    pub fn incoming(&self, f: u32) -> &'a [u32] {
        let i = f as usize;
        if i + 1 >= self.in_off.len() {
            return &[];
        }
        &self.in_from[self.in_off[i] as usize..self.in_off[i + 1] as usize]
    }
}

/// Import edges of a delta segment (FORMAT.md, delta graph section):
/// per delta-local file, the absolute ids it imports, plus the id of the
/// file version it supersedes (`NONE` for a new file). No PageRank: ranks are
/// carried over from the superseded record.
#[derive(Default)]
pub struct DeltaGraphBuilder {
    n: u32,
    prev: Vec<u32>,
    edges: Vec<(u32, u32, u16)>,
}

impl DeltaGraphBuilder {
    pub fn new(n_files: u32) -> Self {
        DeltaGraphBuilder {
            n: n_files,
            prev: vec![NONE; n_files as usize],
            edges: Vec::new(),
        }
    }
    pub fn set_prev(&mut self, file_local: u32, prev_id: u32) {
        if let Some(p) = self.prev.get_mut(file_local as usize) {
            *p = prev_id;
        }
    }
    /// `from` is delta-local, `to` absolute.
    pub fn add(&mut self, from: u32, to: u32, w: u16) {
        if to != NONE && from < self.n {
            self.edges.push((from, to, w));
        }
    }
    pub fn n_edges(&self) -> usize {
        self.edges.len()
    }
    pub fn finish(mut self) -> Vec<u8> {
        let n = self.n as usize;
        self.edges.sort_unstable();
        self.edges.dedup_by(|b, a| {
            if a.0 == b.0 && a.1 == b.1 {
                a.2 = a.2.saturating_add(b.2);
                true
            } else {
                false
            }
        });
        let m = self.edges.len();
        let mut out_off = vec![0u32; n + 1];
        for &(f, _, _) in &self.edges {
            out_off[f as usize + 1] += 1;
        }
        for i in 0..n {
            out_off[i + 1] += out_off[i];
        }
        let out_to: Vec<u32> = self.edges.iter().map(|e| e.1).collect();
        let out_w: Vec<u16> = self.edges.iter().map(|e| e.2).collect();
        let mut body = Vec::with_capacity(16 + n * 8 + m * 6 + 16);
        for x in [self.n, m as u32, 0, 0] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        put_u32s(&mut body, &self.prev);
        put_u32s(&mut body, &out_off);
        put_u32s(&mut body, &out_to);
        body.extend_from_slice(bytemuck::cast_slice(&out_w));
        pad8(&mut body);
        body
    }
}

pub struct DeltaGraphView<'a> {
    pub n: u32,
    /// Superseded file id per delta-local file (`NONE` for new files).
    pub prev: &'a [u32],
    pub out_off: &'a [u32],
    pub out_to: &'a [u32],
    pub out_w: &'a [u16],
}

impl<'a> DeltaGraphView<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        if body.len() < 16 {
            bail!("short delta graph section");
        }
        let h: &[u32] = bytemuck::cast_slice(&body[..16]);
        let (n, m) = (h[0] as usize, h[1] as usize);
        let mut off = 16;
        let prev = take_u32s(body, &mut off, n)?;
        let out_off = take_u32s(body, &mut off, n + 1)?;
        let out_to = take_u32s(body, &mut off, m)?;
        let wb = take_bytes(body, &mut off, m * 2)?;
        let out_w: &[u16] =
            bytemuck::try_cast_slice(wb).map_err(|_| anyhow::anyhow!("unaligned weights"))?;
        if out_off.last().copied().unwrap_or(0) as usize != m
            || out_off.windows(2).any(|w| w[0] > w[1])
        {
            bail!("delta graph offsets corrupt");
        }
        Ok(DeltaGraphView {
            n: n as u32,
            prev,
            out_off,
            out_to,
            out_w,
        })
    }
    pub fn prev(&self, local: u32) -> u32 {
        self.prev.get(local as usize).copied().unwrap_or(NONE)
    }
    /// Absolute ids imported by delta-local file `local`.
    pub fn out(&self, local: u32) -> &'a [u32] {
        let i = local as usize;
        if i + 1 >= self.out_off.len() {
            return &[];
        }
        &self.out_to[self.out_off[i] as usize..self.out_off[i + 1] as usize]
    }
    /// Every edge as (delta-local from, absolute to).
    pub fn edges(&self) -> impl Iterator<Item = (u32, u32)> + 'a {
        let out_off = self.out_off;
        let out_to = self.out_to;
        (0..self.n as usize).flat_map(move |f| {
            out_to[out_off[f] as usize..out_off[f + 1] as usize]
                .iter()
                .map(move |&t| (f as u32, t))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        assert_eq!(split_tokens("getUserName"), vec!["get", "user", "name"]);
        assert_eq!(
            split_tokens("HTTPServerError"),
            vec!["http", "server", "error"]
        );
        assert_eq!(
            split_tokens("parse_json2_fast"),
            vec!["parse", "json2", "fast"]
        );
        assert_eq!(split_tokens("ab"), Vec::<String>::new());
    }

    #[test]
    fn symbols_roundtrip() {
        let src = b"class A:\n    def m(self):\n        pass\n\ndef free():\n    pass\n";
        let ex = greeg_lang::sym::extract(greeg_lang::Lang::Python, false, src);
        let fx = FileExtract::from_extract(ex, src);
        let mut sb = SymBuilder::new(3);
        sb.add_file(0, None);
        sb.add_file(1, Some(&fx));
        let body = sb.finish(&|_| 0.5);
        let v = SymbolsView::parse(&body).unwrap();
        assert_eq!(v.symbols_of(0).1.len(), 0);
        let (base, syms) = v.symbols_of(1);
        assert_eq!(base, 0);
        assert_eq!(syms.len(), 3);
        assert_eq!(v.name(syms[1].name_id), "m");
        assert_eq!(syms[1].parent, 0);
        let a = v.find_name("A").unwrap();
        assert_eq!(v.syms_named(a), &[0]);
        assert!(v.find_name("zzz").is_none());
        assert_eq!(v.enclosing(1, 20), Some(1));
        assert_eq!(v.symbols_of(2).1.len(), 0);
        let mut sp = SpanBuilder::new(3);
        sp.add_file(1, Some(&fx), &[]);
        let pb = sp.finish();
        let pv = SpansView::parse(&pb).unwrap();
        assert!(pv.noncode_of(1).is_empty());
        let mut g = GraphBuilder::new(3);
        g.add(0, 1, 1);
        g.add(2, 1, 3);
        let (gb, rank) = g.finish();
        let gv = GraphView::parse(&gb).unwrap();
        assert_eq!(gv.out(0), &[1]);
        assert_eq!(gv.incoming(1), &[0, 2]);
        assert!(rank[1] > rank[0]);
    }
}
