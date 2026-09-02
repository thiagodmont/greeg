//! Trigram extraction (DESIGN.md §5.1): ASCII case-folded, never spanning a
//! line terminator, deduplicated per file with a 16 Mi-bit bitset.

pub const BITSET_WORDS: usize = (1 << 24) / 64;

pub struct Dedup {
    bits: Vec<u64>,
    touched: Vec<u32>,
}

impl Default for Dedup {
    fn default() -> Self {
        Self::new()
    }
}

impl Dedup {
    pub fn new() -> Self {
        Self { bits: vec![0; BITSET_WORDS], touched: Vec::with_capacity(8192) }
    }

    /// Unique trigram keys of `buf` (already case-folded), sorted ascending.
    pub fn extract(&mut self, buf: &[u8], out: &mut Vec<u32>) {
        out.clear();
        let n = buf.len();
        if n < 3 {
            return;
        }
        let mut i = 0usize;
        while i + 2 < n {
            let (a, b, c) = (buf[i], buf[i + 1], buf[i + 2]);
            if c == b'\n' {
                i += 3;
                continue;
            }
            if b == b'\n' {
                i += 2;
                continue;
            }
            if a == b'\n' {
                i += 1;
                continue;
            }
            let key = key3(a, b, c);
            let w = (key >> 6) as usize;
            let m = 1u64 << (key & 63);
            if self.bits[w] & m == 0 {
                if self.bits[w] == 0 {
                    self.touched.push(w as u32);
                }
                self.bits[w] |= m;
                out.push(key);
            }
            i += 1;
        }
        for &w in &self.touched {
            self.bits[w as usize] = 0;
        }
        self.touched.clear();
        out.sort_unstable();
    }
}

#[inline]
pub fn key3(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

#[inline]
pub fn fold(b: u8) -> u8 {
    if b.is_ascii_uppercase() { b | 0x20 } else { b }
}

pub fn fold_buf(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        if b.is_ascii_uppercase() {
            *b |= 0x20;
        }
    }
}

/// Trigram keys of a literal (case-folded), split at line terminators;
/// returns one AND-group per piece of length ≥ 3, `None` if no piece qualifies.
pub fn literal_keys(lit: &[u8]) -> Vec<Vec<u32>> {
    let mut groups = Vec::new();
    for piece in lit.split(|&b| b == b'\n') {
        if piece.len() < 3 {
            continue;
        }
        let mut v: Vec<u32> = piece.windows(3).map(|w| key3(fold(w[0]), fold(w[1]), fold(w[2]))).collect();
        v.sort_unstable();
        v.dedup();
        groups.push(v);
    }
    groups
}
