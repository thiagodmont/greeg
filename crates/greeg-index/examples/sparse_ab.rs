//! M4 A/B harness: gram schemes vs trigrams on a corpus.
//!
//!   sparse_ab train OUT.bin CORPUS...          bigram weight table (65536 × u16 LE, rarer = heavier)
//!   sparse_ab eval WEIGHTS.bin CORPUS [--size] per scheme: pairs, distinct grams, extraction MB/s,
//!                                              candidates for the identifier family, scans forced
//!
//! Schemes: tri (baseline), cox{min,max} (boundary bigrams heavier than every
//! interior bigram; min 3 includes every trigram), minz{k,w} (minimizers:
//! the heaviest k-mer of every window of w k-mers, weight = rarest bigram
//! inside, hash tie-break). Query grams use only windows inside the literal,
//! so query grams ⊆ document grams by construction; a literal that yields no
//! gram is a full scan.

use greeg_index::build::walk;
use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

type W = Vec<u16>; // 65536 bigram weights

fn fold_read(path: &Path) -> Option<Vec<u8>> {
    let mut b = std::fs::read(path).ok()?;
    if b.len() > 4 << 20 || b[..b.len().min(8192)].contains(&0) {
        return None;
    }
    for x in b.iter_mut() {
        if x.is_ascii_uppercase() {
            *x |= 0x20;
        }
    }
    Some(b)
}

fn files_of(root: &Path) -> Vec<PathBuf> {
    let (w, _) = walk(root).unwrap();
    w.into_iter().map(|f| root.join(f.rel)).collect()
}

// ---------------------------------------------------------------- weights

fn train(out: &Path, corpora: &[PathBuf]) {
    let counts: Mutex<Vec<u64>> = Mutex::new(vec![0u64; 65536]);
    for c in corpora {
        let files = files_of(c);
        files.par_iter().for_each(|p| {
            let Some(b) = fold_read(p) else { return };
            let mut local = vec![0u64; 65536];
            for w in b.windows(2) {
                if w[0] != b'\n' && w[1] != b'\n' {
                    local[(w[0] as usize) << 8 | w[1] as usize] += 1;
                }
            }
            let mut g = counts.lock().unwrap();
            for i in 0..65536 {
                g[i] += local[i];
            }
        });
    }
    let counts = counts.into_inner().unwrap();
    // rank by count ascending: rarest gets the highest weight; equal counts share a weight
    let mut order: Vec<usize> = (0..65536).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    let mut w = vec![0u16; 65536];
    let mut rank = 0u16;
    let mut prev = u64::MAX;
    for &i in &order {
        if counts[i] != prev {
            rank = rank.saturating_add(1);
            prev = counts[i];
        }
        w[i] = rank;
    }
    // spread ranks over the u16 range (ranks are dense, up to a few thousand distinct counts)
    let max = *w.iter().max().unwrap() as u32;
    for x in w.iter_mut() {
        *x = ((*x as u32 * 65535) / max.max(1)) as u16;
    }
    let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    std::fs::write(out, bytes).unwrap();
    let nz = counts.iter().filter(|&&c| c > 0).count();
    println!(
        "trained on {} corpora: {} bigrams seen, weights written to {}",
        corpora.len(),
        nz,
        out.display()
    );
}

fn load_weights(p: &Path) -> W {
    let b = std::fs::read(p).unwrap();
    b.chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[inline]
fn wg(w: &W, a: u8, b: u8) -> u16 {
    w[(a as usize) << 8 | b as usize]
}

#[inline]
fn h64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b.len() as u64;
    h ^= h >> 32;
    h = h.wrapping_mul(0x9E3779B97F4A7C15);
    h ^= h >> 29;
    h
}

// ---------------------------------------------------------------- schemes

#[derive(Clone, Copy, Debug)]
enum Scheme {
    Tri,
    Cox { min: usize, max: usize },
    Minz { k: usize, w: usize },
}

impl Scheme {
    fn name(&self) -> String {
        match self {
            Scheme::Tri => "tri".into(),
            Scheme::Cox { min, max } => format!("cox{min}-{max}"),
            Scheme::Minz { k, w } => format!("minz k{k} w{w}"),
        }
    }
}

/// Grams of a (folded) byte string under a scheme. Same function for
/// documents and query literals (the rules are local).
fn grams(s: Scheme, b: &[u8], w: &W, out: &mut Vec<u64>) {
    out.clear();
    let n = b.len();
    match s {
        Scheme::Tri => {
            for i in 0..n.saturating_sub(2) {
                let t = &b[i..i + 3];
                if !t.contains(&b'\n') {
                    out.push(((t[0] as u64) << 16) | ((t[1] as u64) << 8) | t[2] as u64);
                }
            }
        }
        Scheme::Cox { min, max } => {
            for i in 0..n.saturating_sub(min - 1) {
                let mut interior = 0u32; // max interior weight + 1 (0 = empty)
                let w0 = wg(w, b[i], b[i + 1]) as u32;
                if b[i] == b'\n' || b[i + 1] == b'\n' {
                    continue;
                }
                let mut j = i + 2;
                while j < n && j - i < max {
                    if b[j] == b'\n' {
                        break;
                    }
                    let wl = wg(w, b[j - 1], b[j]) as u32;
                    if j - i + 1 >= min && w0.min(wl) + 1 > interior {
                        out.push(h64(&b[i..=j]));
                    }
                    // gap j-1 becomes interior for longer grams
                    interior = interior.max(wl + 1);
                    j += 1;
                }
            }
        }
        Scheme::Minz { k, w: win } => {
            if n < k {
                return;
            }
            // k-mer weights: (rarest bigram inside, hash) as a u64 key; '\n' inside => invalid
            let m = n - k + 1;
            let mut wt: Vec<u64> = Vec::with_capacity(m);
            for i in 0..m {
                let km = &b[i..i + k];
                if km.contains(&b'\n') {
                    wt.push(0);
                    continue;
                }
                let mut best = 0u16;
                for p in km.windows(2) {
                    best = best.max(wg(w, p[0], p[1]));
                }
                wt.push(((best as u64) << 48) | (h64(km) & 0xffff_ffff_ffff));
            }
            let mut last_sel = usize::MAX;
            for start in 0..m.saturating_sub(win - 1) {
                let mut bi = start;
                for i in start..start + win {
                    if wt[i] > wt[bi] {
                        bi = i;
                    }
                }
                if wt[bi] != 0 && bi != last_sel {
                    last_sel = bi;
                    out.push(h64(&b[bi..bi + k]));
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
}

// ---------------------------------------------------------------- eval

struct Eval {
    files: usize,
    bytes: u64,
    pairs: u64,
    distinct_sample: usize, // distinct grams with hash % 64 == 0
    cpu_ns: u128,
    postings_bytes: u64,
    dict_entries: u64,
    cands: Vec<u32>, // per query
    scans: usize,
}

fn identifier_queries(root: &Path) -> Vec<String> {
    let mut q: Vec<String> = Vec::new();
    // fixed identifier family from the S1 spike
    for s in [
        "createSourceFile",
        "checkExpression",
        "ParseFlags",
        "mir_borrowck",
        "HirId",
        "TyCtxt",
        "get_queryset",
        "HttpResponseRedirect",
        "request",
        "respond",
        "ContentNegotiation",
        "JoinHandle",
        "Semaphore",
        "spawn_blocking",
        "poll_next",
        "LocalDefId",
        "Symbol",
        "Model",
    ] {
        q.push(s.to_string());
    }
    // sample symbol names from the index (every k-th name of length >= 4)
    if let Ok(dir) = greeg_index::index_dir_for(root)
        && let Ok(idx) = greeg_index::Index::open(&dir)
        && let Some(sv) = idx.base.symbols()
    {
        let n = sv.n_names();
        let step = (n / 200).max(1);
        let mut i = 0;
        while i < n && q.len() < 250 {
            let name = sv.name(i as u32);
            if name.len() >= 4 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                q.push(name.to_string());
            }
            i += step;
        }
    }
    q
}

fn eval(weights: &Path, root: &Path, size: bool) {
    let w = load_weights(weights);
    let files = files_of(root);
    let queries = identifier_queries(root);
    let schemes = [
        Scheme::Tri,
        Scheme::Cox { min: 3, max: 8 },
        Scheme::Cox { min: 4, max: 8 },
        Scheme::Minz { k: 4, w: 3 },
        Scheme::Minz { k: 5, w: 3 },
        Scheme::Minz { k: 5, w: 4 },
        Scheme::Minz { k: 6, w: 4 },
        Scheme::Minz { k: 6, w: 6 },
    ];
    println!(
        "== {} : {} files, {} identifier queries",
        root.display(),
        files.len(),
        queries.len()
    );
    // query grams per scheme (folded)
    let qg: Vec<Vec<Option<Vec<u64>>>> = schemes
        .iter()
        .map(|&s| {
            queries
                .iter()
                .map(|q| {
                    let lit: Vec<u8> = q
                        .bytes()
                        .map(|b| if b.is_ascii_uppercase() { b | 0x20 } else { b })
                        .collect();
                    let mut v = Vec::new();
                    grams(s, &lit, &w, &mut v);
                    if v.is_empty() { None } else { Some(v) }
                })
                .collect()
        })
        .collect();
    let mut results: Vec<Eval> = schemes
        .iter()
        .map(|_| Eval {
            files: 0,
            bytes: 0,
            pairs: 0,
            distinct_sample: 0,
            cpu_ns: 0,
            postings_bytes: 0,
            dict_entries: 0,
            cands: vec![0; queries.len()],
            scans: 0,
        })
        .collect();
    struct FileOut {
        bytes: u64,
        cpu: u128,
        pairs: u64,
        hits: Vec<u16>,
        sample: Vec<u64>,
        keys: Option<Vec<u64>>,
    }
    for (si, &s) in schemes.iter().enumerate() {
        let outs: Vec<Option<FileOut>> = files
            .par_iter()
            .map_init(
                || (Vec::<u64>::with_capacity(1 << 16), HashSet::<u64>::new()),
                |(g, set), p| {
                    let b = fold_read(p)?;
                    let t = Instant::now();
                    grams(s, &b, &w, g);
                    let cpu = t.elapsed().as_nanos();
                    set.clear();
                    set.extend(g.iter().copied());
                    let mut hits = Vec::new();
                    for (qi, qq) in qg[si].iter().enumerate() {
                        let hit = match qq {
                            None => true,
                            Some(v) => v.iter().all(|k| set.contains(k)),
                        };
                        if hit {
                            hits.push(qi as u16);
                        }
                    }
                    let sample: Vec<u64> = g.iter().copied().filter(|k| k % 64 == 0).collect();
                    Some(FileOut {
                        bytes: b.len() as u64,
                        cpu,
                        pairs: g.len() as u64,
                        hits,
                        sample,
                        keys: if size { Some(g.clone()) } else { None },
                    })
                },
            )
            .collect();
        let r = &mut results[si];
        let mut sample: HashSet<u64> = HashSet::new();
        let mut post: HashMap<u64, Vec<u32>> = HashMap::new();
        for (fid, o) in outs.into_iter().enumerate() {
            let Some(o) = o else { continue };
            r.files += 1;
            r.bytes += o.bytes;
            r.cpu_ns += o.cpu;
            r.pairs += o.pairs;
            for qi in o.hits {
                r.cands[qi as usize] += 1;
            }
            sample.extend(o.sample);
            if let Some(keys) = o.keys {
                for k in keys {
                    post.entry(k).or_default().push(fid as u32);
                }
            }
        }
        r.scans = qg[si].iter().filter(|q| q.is_none()).count();
        r.distinct_sample = sample.len();
        if size {
            let mut bytes = 0u64;
            for (_, v) in post.iter() {
                let bm = RoaringBitmap::from_sorted_iter(v.iter().copied()).unwrap();
                bytes += bm.serialized_size() as u64;
            }
            r.postings_bytes = bytes;
            r.dict_entries = post.len() as u64;
        }
        let mbs = r.bytes as f64 / 1e6 / (r.cpu_ns as f64 / 1e9);
        println!(
            "{:<12} pairs {:>11}  pairs/byte {:.3}  ~distinct {:>9}  extract {:>6.0} MB/s/core  scans {:>3}/{}{}",
            s.name(),
            r.pairs,
            r.pairs as f64 / r.bytes as f64,
            r.distinct_sample * 64,
            mbs,
            r.scans,
            queries.len(),
            if size {
                format!(
                    "  postings {:.1} MB  dict {}",
                    r.postings_bytes as f64 / 1e6,
                    r.dict_entries
                )
            } else {
                String::new()
            }
        );
    }
    // candidates: per query table for the fixed family, and summary ratios over the sample
    println!("\ncandidates (files) per scheme:");
    print!("{:<24}", "query");
    for s in &schemes {
        print!("{:>12}", s.name());
    }
    println!();
    for (qi, q) in queries.iter().enumerate().take(18) {
        print!("{:<24}", q);
        for r in &results {
            print!("{:>12}", r.cands[qi]);
        }
        println!();
    }
    let base: Vec<f64> = results[0].cands.iter().map(|&c| c as f64).collect();
    println!(
        "\nsummary over {} identifier queries (geometric mean of candidates / trigram candidates; lower is better):",
        queries.len()
    );
    for (si, r) in results.iter().enumerate() {
        let mut lg = 0f64;
        let mut n = 0;
        let mut worse = 0;
        for (qi, &c) in r.cands.iter().enumerate() {
            if base[qi] > 0.0 {
                lg += ((c as f64).max(1.0) / base[qi].max(1.0)).ln();
                n += 1;
                if c as f64 > base[qi] * 1.05 {
                    worse += 1;
                }
            }
        }
        println!(
            "{:<12} ratio {:.3}  queries worse than trigram: {}  scans: {}  pairs vs tri: {:.2}",
            schemes[si].name(),
            (lg / n.max(1) as f64).exp(),
            worse,
            r.scans,
            r.pairs as f64 / results[0].pairs.max(1) as f64
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("train") => train(
            Path::new(&args[2]),
            &args[3..].iter().map(PathBuf::from).collect::<Vec<_>>(),
        ),
        Some("eval") => eval(
            Path::new(&args[2]),
            Path::new(&args[3]),
            args.iter().any(|a| a == "--size"),
        ),
        _ => {
            eprintln!("usage: sparse_ab train OUT.bin CORPUS... | eval WEIGHTS.bin CORPUS [--size]")
        }
    }
}
