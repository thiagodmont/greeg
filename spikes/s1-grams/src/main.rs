//! S1: file-level trigram postings — size ratio, build throughput, selectivity.
//! usage: s1-grams ROOT [--threads N] [--q ident1,ident2,...]
use anyhow::Result;
use hashbrown::HashMap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

const MAX_FILE: u64 = 4 << 20;
const BITSET_WORDS: usize = (1 << 24) / 64; // 16M trigram keys

struct Dedup {
    bits: Vec<u64>,
    touched: Vec<u32>,
}
impl Dedup {
    fn new() -> Self {
        Self { bits: vec![0; BITSET_WORDS], touched: Vec::with_capacity(8192) }
    }
    /// Extract unique trigram keys from case-folded bytes; grams never span '\n'.
    fn extract(&mut self, buf: &[u8], out: &mut Vec<u32>) {
        out.clear();
        if buf.len() < 3 {
            return;
        }
        let mut i = 0usize;
        let n = buf.len();
        while i + 2 < n {
            let (a, b, c) = (buf[i], buf[i + 1], buf[i + 2]);
            if c == b'\n' { i += 3; continue; }
            if b == b'\n' { i += 2; continue; }
            if a == b'\n' { i += 1; continue; }
            let key = ((a as u32) << 16) | ((b as u32) << 8) | (c as u32);
            let w = (key >> 6) as usize;
            let m = 1u64 << (key & 63);
            if self.bits[w] & m == 0 {
                if self.bits[w] == 0 { self.touched.push(w as u32); }
                self.bits[w] |= m;
                out.push(key);
            }
            i += 1;
        }
        for &w in &self.touched { self.bits[w as usize] = 0; }
        self.touched.clear();
        out.sort_unstable();
    }
}

fn fold_ascii(buf: &mut [u8]) {
    for b in buf.iter_mut() { if b.is_ascii_uppercase() { *b |= 0x20; } }
}
fn is_binary(buf: &[u8]) -> bool { memchr::memchr(0, &buf[..buf.len().min(8192)]).is_some() }

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(&args[0]);
    let mut threads = 4usize;
    let mut queries: Vec<String> = vec![];
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--threads" => { threads = args[i + 1].parse()?; i += 2; }
            "--q" => { queries = args[i + 1].split(',').map(|s| s.to_string()).collect(); i += 2; }
            _ => i += 1,
        }
    }
    rayon::ThreadPoolBuilder::new().num_threads(threads).build_global()?;

    let t0 = Instant::now();
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(&root).hidden(true).build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.into_path()).collect();
    files.sort();
    let walk_ms = t0.elapsed().as_secs_f64() * 1e3;

    let read_ns = AtomicU64::new(0);
    let extract_ns = AtomicU64::new(0);
    let bytes_total = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let t1 = Instant::now();

    let merged: HashMap<u32, Vec<u32>> = files.par_iter().enumerate()
        .fold(
            || (HashMap::<u32, Vec<u32>>::with_capacity(1 << 16), Dedup::new(), Vec::<u32>::with_capacity(8192)),
            |(mut map, mut dd, mut grams), (id, path)| {
                let tr = Instant::now();
                let Ok(md) = fs::metadata(path) else { return (map, dd, grams) };
                if md.len() > MAX_FILE || md.len() == 0 { skipped.fetch_add(1, Relaxed); return (map, dd, grams); }
                let Ok(mut buf) = fs::read(path) else { return (map, dd, grams) };
                read_ns.fetch_add(tr.elapsed().as_nanos() as u64, Relaxed);
                if is_binary(&buf) { skipped.fetch_add(1, Relaxed); return (map, dd, grams); }
                let te = Instant::now();
                fold_ascii(&mut buf);
                dd.extract(&buf, &mut grams);
                for &g in &grams { map.entry(g).or_insert_with(|| Vec::with_capacity(8)).push(id as u32); }
                extract_ns.fetch_add(te.elapsed().as_nanos() as u64, Relaxed);
                bytes_total.fetch_add(buf.len() as u64, Relaxed);
                (map, dd, grams)
            },
        )
        .map(|(m, _, _)| m)
        .reduce(HashMap::new, |mut a, b| { for (k, mut v) in b { a.entry(k).or_default().append(&mut v); } a });
    let build_ms = t1.elapsed().as_secs_f64() * 1e3;

    let t2 = Instant::now();
    let mut postings: Vec<(u32, RoaringBitmap)> = merged.into_iter().collect::<Vec<(u32, Vec<u32>)>>().into_par_iter()
        .map(|(k, mut v)| { v.sort_unstable(); (k, RoaringBitmap::from_sorted_iter(v).unwrap()) }).collect();
    postings.par_sort_unstable_by_key(|(k, _)| *k);
    let postings_bytes: u64 = postings.par_iter().map(|(_, b)| b.serialized_size() as u64).sum();
    let dict_bytes = postings.len() as u64 * 16;
    let bitmap_ms = t2.elapsed().as_secs_f64() * 1e3;

    let src = bytes_total.load(Relaxed);
    let rd = read_ns.load(Relaxed) as f64 / 1e9;
    let ex = extract_ns.load(Relaxed) as f64 / 1e9;
    println!("root={} files={} indexed_bytes={:.1}MB skipped={} threads={}", root.display(), files.len(), src as f64 / 1e6, skipped.load(Relaxed), threads);
    println!("walk={:.0}ms  build(read+extract+map)={:.0}ms  bitmaps+merge={:.0}ms  total={:.0}ms", walk_ms, build_ms, bitmap_ms, walk_ms + build_ms + bitmap_ms);
    println!("cpu: read={:.2}s ({:.0} MB/s/core)  extract={:.2}s ({:.0} MB/s/core)", rd, src as f64 / 1e6 / rd, ex, src as f64 / 1e6 / ex);
    println!("distinct_grams={}  postings={:.1}MB  dict={:.1}MB  index/source={:.3}", postings.len(), postings_bytes as f64 / 1e6, dict_bytes as f64 / 1e6, (postings_bytes + dict_bytes) as f64 / src as f64);
    let mut dc: Vec<u64> = postings.iter().map(|(_, b)| b.len()).collect();
    dc.sort_unstable();
    let pct = |p: f64| dc[((dc.len() - 1) as f64 * p) as usize];
    println!("posting doc-count: p50={} p90={} p99={} max={} ({} files)", pct(0.5), pct(0.9), pct(0.99), dc[dc.len() - 1], files.len());

    if !queries.is_empty() {
        let lookup = |k: u32| postings.binary_search_by_key(&k, |(g, _)| *g).ok().map(|i| &postings[i].1);
        println!("\n{:<28} {:>6} {:>7} {:>9} {:>8} {:>6} {:>9} {:>8}", "query", "grams", "rarest", "cand_all", "cand_k6", "true", "verify_ms", "plan_us");
        for q in &queries {
            let mut qb = q.as_bytes().to_vec();
            fold_ascii(&mut qb);
            let mut dd = Dedup::new();
            let mut g = vec![];
            dd.extract(&qb, &mut g);
            let mut lists: Vec<&RoaringBitmap> = g.iter().filter_map(|&k| lookup(k)).collect();
            if lists.len() < g.len() { println!("{:<28} some gram absent -> 0 candidates", q); continue; }
            lists.sort_by_key(|b| b.len());
            let tq = Instant::now();
            let mut acc = lists[0].clone();
            for b in &lists[1..] { acc &= *b; if acc.is_empty() { break; } }
            let cand_all = acc.len();
            let mut acc6 = lists[0].clone();
            for b in lists.iter().take(6).skip(1) { acc6 &= *b; }
            let cand6 = acc6.len();
            let plan_us = tq.elapsed().as_micros();
            let tv = Instant::now();
            let finder = memchr::memmem::Finder::new(&qb);
            let truth: usize = acc.iter().par_bridge().map(|id| {
                let Ok(mut b) = fs::read(&files[id as usize]) else { return 0 };
                fold_ascii(&mut b);
                finder.find(&b).is_some() as usize
            }).sum();
            let ver_ms = tv.elapsed().as_secs_f64() * 1e3;
            println!("{:<28} {:>6} {:>7} {:>9} {:>8} {:>6} {:>9.1} {:>8}", q, g.len(), lists[0].len(), cand_all, cand6, truth, ver_ms, plan_us);
        }
    }
    Ok(())
}
