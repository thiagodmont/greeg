//! Walk a tree, run stage-B extraction on every file with a grammar, report throughput.
use greeg_lang::{Lang, sym};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default, Clone)]
struct St {
    files: usize,
    bytes: u64,
    cpu_ns: u128,
    fallback: usize,
    errors: usize,
    symbols: usize,
    imports: usize,
    noncode: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s == "--agree").unwrap_or(false) {
        agree(Path::new(&args[2]));
        return;
    }
    let root = PathBuf::from(std::env::args().nth(1).expect("root"));
    let files: Vec<(PathBuf, Lang)> = ignore::WalkBuilder::new(&root)
        .hidden(true)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let l = Lang::from_path(e.path());
            if l.has_grammar() {
                Some((e.into_path(), l))
            } else {
                None
            }
        })
        .collect();
    sym::warm();
    let per: Mutex<std::collections::BTreeMap<&'static str, St>> = Mutex::new(Default::default());
    let t0 = Instant::now();
    files.par_iter().for_each(|(p, l)| {
        let Ok(src) = std::fs::read(p) else { return };
        if src.len() > 4 << 20 || src.contains(&0) {
            return;
        }
        let t = Instant::now();
        let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
        let ex = sym::extract(*l, sym::is_tsx(&rel), &src);
        let dt = t.elapsed().as_nanos();
        let mut g = per.lock().unwrap();
        let s = g.entry(l.name()).or_default();
        s.files += 1;
        s.bytes += src.len() as u64;
        s.cpu_ns += dt;
        s.fallback += (!ex.tree_sitter) as usize;
        s.errors += ex.parse_errors as usize;
        s.symbols += ex.symbols.len();
        s.imports += ex.imports.len();
        s.noncode += ex.noncode.len();
    });
    let wall = t0.elapsed();
    let g = per.lock().unwrap();
    let mut tot = St::default();
    for (l, s) in g.iter() {
        println!(
            "{:<11} files {:>6}  MB {:>7.1}  cpu {:>6.2} s  {:>5.1} MB/s/core  fallback {:>4} ({:.1}%)  errors {:>5} ({:.1}%)  symbols {:>7}  imports {:>6}  noncode {:>7}",
            l,
            s.files,
            s.bytes as f64 / 1e6,
            s.cpu_ns as f64 / 1e9,
            s.bytes as f64 / 1e6 / (s.cpu_ns as f64 / 1e9),
            s.fallback,
            100.0 * s.fallback as f64 / s.files as f64,
            s.errors,
            100.0 * s.errors as f64 / s.files as f64,
            s.symbols,
            s.imports,
            s.noncode
        );
        tot.files += s.files;
        tot.bytes += s.bytes;
        tot.cpu_ns += s.cpu_ns;
        tot.symbols += s.symbols;
    }
    println!(
        "total {} files {:.1} MB, wall {:.0} ms on {} threads, cpu {:.2} s, {} symbols",
        tot.files,
        tot.bytes as f64 / 1e6,
        wall.as_secs_f64() * 1e3,
        rayon::current_num_threads(),
        tot.cpu_ns as f64 / 1e9,
        tot.symbols
    );
    let _ = Path::new("");
}

/// Agreement between the tree-sitter extractor and the regex fallback on
/// definition names: run with `--agree ROOT`.
#[allow(dead_code)]
pub fn agree(root: &Path) {
    use greeg_lang::{defs, lexer};
    let files: Vec<(PathBuf, Lang)> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let l = Lang::from_path(e.path());
            if l.has_grammar() {
                Some((e.into_path(), l))
            } else {
                None
            }
        })
        .collect();
    type Agree = (usize, usize, usize, usize);
    let per: Mutex<std::collections::BTreeMap<&'static str, Agree>> =
        Mutex::new(Default::default());
    files.par_iter().for_each(|(p, l)| {
        let Ok(src) = std::fs::read(p) else { return };
        if src.len() > 4 << 20 || src.contains(&0) {
            return;
        }
        let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy();
        let ex = sym::extract(*l, sym::is_tsx(&rel), &src);
        if !ex.tree_sitter {
            return;
        }
        let lexed = lexer::lex(*l, &src);
        let ol = defs::outline(*l, &src, &lexed);
        // compare (name, line) sets for container/function kinds both extractors know
        let ts: std::collections::HashSet<(String, u32)> = ex
            .symbols
            .iter()
            .filter(|s| {
                !matches!(
                    s.kind,
                    greeg_lang::DefKind::Field
                        | greeg_lang::DefKind::Variant
                        | greeg_lang::DefKind::Impl
                        | greeg_lang::DefKind::Variable
                        | greeg_lang::DefKind::Constant
                )
            })
            .map(|s| (ex.name(s, &src).to_string(), s.line))
            .collect();
        let rx: std::collections::HashSet<(String, u32)> = ol
            .defs
            .iter()
            .filter(|d| {
                !matches!(
                    d.kind,
                    greeg_lang::DefKind::Impl
                        | greeg_lang::DefKind::Variable
                        | greeg_lang::DefKind::Constant
                )
            })
            .map(|d| {
                (
                    String::from_utf8_lossy(&src[d.name_start as usize..d.name_end as usize])
                        .to_string(),
                    d.line,
                )
            })
            .collect();
        let both = ts.intersection(&rx).count();
        let mut g = per.lock().unwrap();
        let e = g.entry(l.name()).or_default();
        e.0 += ts.len();
        e.1 += rx.len();
        e.2 += both;
        e.3 += 1;
    });
    for (l, (ts, rx, both, n)) in per.lock().unwrap().iter() {
        println!(
            "{:<11} files {:>6}  tree-sitter defs {:>7}  regex defs {:>7}  agree {:>7}  regex∩ts/regex {:>5.1}%  regex∩ts/ts {:>5.1}%",
            l,
            n,
            ts,
            rx,
            both,
            100.0 * *both as f64 / (*rx).max(1) as f64,
            100.0 * *both as f64 / (*ts).max(1) as f64
        );
    }
}
