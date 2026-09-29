//! Files added or deleted by a delta move the edges of files that did not
//! change: an import that now resolves, resolves to a closer file, or falls
//! back to another. After every delta, the graph equals a clean build's.

use greeg_index::Index;
use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

struct Tmp {
    base: PathBuf,
    root: PathBuf,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn w(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn opts() -> BuildOpts {
    BuildOpts {
        reader_threads: 2,
        quiet: true,
        ..Default::default()
    }
}

fn tree(files: &[(&str, &str)]) -> Tmp {
    let base = std::env::temp_dir().join(format!(
        "greeg-graph-repair-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("tree");
    fs::create_dir_all(root.join(".git")).unwrap();
    for (name, body) in files {
        w(&root.join(name), body);
    }
    // enough files that each step stays a delta
    for i in 0..400 {
        w(&root.join(format!("filler/f{i:03}.txt")), "filler\n");
    }
    Tmp { base, root }
}

/// Every edge as (importer, imported) paths.
fn edges(idx: &Index) -> BTreeSet<(String, String)> {
    let path = |id| String::from_utf8_lossy(idx.path(id).unwrap()).into_owned();
    let mut out = BTreeSet::new();
    for (id, _, _) in idx.live_files() {
        for &to in idx.out_edges(id).iter() {
            out.insert((path(id), path(to)));
        }
    }
    out
}

/// Files whose extraction differs between two indexes of the same bytes: a
/// parse that runs out of time under load falls back to regex extraction,
/// which may find other imports.
fn unsettled(a: &Index, b: &Index) -> BTreeSet<String> {
    let flags = |idx: &Index| -> BTreeSet<(String, bool)> {
        idx.live_files()
            .map(|(_, rel, rec)| {
                let fallback = rec.flags & greeg_lang::FileFlags::PARSE_ERRORS != 0;
                (String::from_utf8_lossy(rel).into_owned(), fallback)
            })
            .collect()
    };
    flags(a)
        .symmetric_difference(&flags(b))
        .map(|(rel, _)| rel.clone())
        .collect()
}

/// Publish what changed as a delta, then compare its graph with a clean
/// build of the same tree.
fn step(t: &Tmp, dir: &Path, what: &str) {
    let idx = Index::open(dir).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let ch = fresh::check(&idx, &t.root, Mode::Stat, 2).unwrap();
    assert_eq!(fresh::rebuild_reason(&idx, &ch), None, "{what}: rebuilds");
    fresh::apply(&idx, &t.root, &ch).unwrap();
    let delta = Index::open(dir).unwrap();
    assert!(!delta.deltas.is_empty(), "{what}: no delta");
    let clean_dir = t
        .base
        .join(format!("clean-{}", N.fetch_add(1, Ordering::Relaxed)));
    build(&t.root, &clean_dir, &opts()).unwrap();
    let clean = Index::open(&clean_dir).unwrap();
    assert_eq!(edges(&delta), edges(&clean), "{what}");
}

#[test]
fn edges_after_added_and_deleted_files_match_a_clean_build() {
    let t = tree(&[
        ("app.py", "import helpers\nfrom pkg import mod\n"),
        ("pkg/__init__.py", ""),
        (
            "ts/main.ts",
            "import { a } from './a';\nimport { b } from './b';\n",
        ),
        ("ts/a.js", "export const a = 1;\n"),
        ("src/lib.rs", "mod net;\nuse crate::net::conn;\n"),
        ("src/net/mod.rs", "pub mod conn;\n"),
    ]);
    let dir = t.base.join("index");
    build(&t.root, &dir, &opts()).unwrap();
    let built = edges(&Index::open(&dir).unwrap());
    for e in [("app.py", "pkg/__init__.py"), ("ts/main.ts", "ts/a.js")] {
        assert!(built.contains(&(e.0.into(), e.1.into())), "{built:?}");
    }

    // the importer itself then lives in a delta
    w(
        &t.root.join("app.py"),
        "import helpers\nfrom pkg import mod\n# edited\n",
    );
    step(&t, &dir, "an edited importer");
    w(&t.root.join("helpers.py"), "def help():\n    pass\n");
    step(&t, &dir, "an added target");
    w(&t.root.join("ts/a.ts"), "export const a = 2;\n");
    step(&t, &dir, "an added file that shadows the target");
    w(&t.root.join("src/net.rs"), "pub mod conn;\n");
    step(&t, &dir, "an added module file that shadows mod.rs");
    fs::remove_file(t.root.join("ts/a.ts")).unwrap();
    step(&t, &dir, "a deleted target with a fallback");
    fs::remove_file(t.root.join("helpers.py")).unwrap();
    step(&t, &dir, "a deleted target");
    w(&t.root.join("helpers.py"), "def help():\n    pass\n");
    step(&t, &dir, "a recreated target");
    fs::rename(t.root.join("ts/a.js"), t.root.join("ts/b.js")).unwrap();
    step(&t, &dir, "a renamed target");
}

#[test]
fn edges_through_aliases_and_packages_match_a_clean_build() {
    let t = tree(&[
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["src/*"], "@x": ["src/x/impl.ts"] } } }"#,
        ),
        (
            "package.json",
            r#"{ "name": "mono", "workspaces": ["packages/*"] }"#,
        ),
        (
            "packages/a/package.json",
            r#"{ "name": "pkg-a", "main": "dist/start.js" }"#,
        ),
        (
            "src/main.ts",
            "import '@app/feature';\nimport '@x';\nimport 'lib/util';\nimport 'pkg-a';\n",
        ),
    ]);
    let dir = t.base.join("index");
    build(&t.root, &dir, &opts()).unwrap();
    w(&t.root.join("src/feature.ts"), "export {};\n");
    step(&t, &dir, "an alias ending in *");
    w(&t.root.join("src/x/impl.ts"), "export {};\n");
    step(&t, &dir, "an exact alias target");
    w(&t.root.join("lib/util.ts"), "export {};\n");
    step(&t, &dir, "a baseUrl import");
    w(&t.root.join("packages/a/src/index.ts"), "export {};\n");
    step(&t, &dir, "a package's fallback entry");
    w(&t.root.join("packages/a/dist/start.ts"), "export {};\n");
    step(&t, &dir, "a package's main, shadowing its fallback");
    fs::remove_file(t.root.join("src/x/impl.ts")).unwrap();
    step(&t, &dir, "a deleted exact alias target");
    let idx = Index::open(&dir).unwrap();
    assert_eq!(edges(&idx).len(), 3, "{:?}", edges(&idx));
}

/// A file of the tree that other files import, and a shadowing sibling a new
/// file would take the imports from.
fn shadow_of(rel: &str) -> Option<String> {
    if let Some(stem) = rel.strip_suffix(".js") {
        return Some(format!("{stem}.ts"));
    }
    if let Some(dir) = rel.strip_suffix("/mod.rs") {
        return Some(format!("{dir}.rs"));
    }
    if let Some(dir) = rel.strip_suffix("/__init__.py") {
        return Some(format!("{dir}.py"));
    }
    if let Some(dir) = rel.strip_suffix("/index.ts") {
        return Some(format!("{dir}.ts"));
    }
    None
}

/// The file that makes the directory of `rel` a module of its own.
fn module_of_dir(rel: &str) -> Option<String> {
    let (dir, name) = rel.rsplit_once('/')?;
    let module = match name.rsplit_once('.')?.1 {
        "py" => "__init__.py",
        "rs" => "mod.rs",
        "ts" | "tsx" => "index.ts",
        "js" | "jsx" => "index.js",
        _ => return None,
    };
    Some(format!("{dir}/{module}"))
}

/// Seeded random adds, deletes, recreations, renames and shadowing files on a
/// copy of a real tree; after each delta the graph equals a clean build's.
/// `GREEG_GRAPH_CORPUS=<tree> cargo test --release --test graph_repair -- --ignored`
#[test]
#[ignore = "needs GREEG_GRAPH_CORPUS, a source tree to copy"]
fn random_membership_changes_on_a_real_tree_match_a_clean_build() {
    let src = PathBuf::from(std::env::var_os("GREEG_GRAPH_CORPUS").expect("GREEG_GRAPH_CORPUS"));
    let steps: usize = std::env::var("GREEG_GRAPH_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let t = tree(&[]);
    fs::remove_dir_all(&t.root).unwrap();
    let copied = std::process::Command::new("cp")
        .arg("-R")
        .arg(&src)
        .arg(&t.root)
        .status()
        .unwrap();
    assert!(copied.success());
    let dir = t.base.join("index");
    build(&t.root, &dir, &opts()).unwrap();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let mut gone: Vec<(String, Vec<u8>)> = Vec::new();
    let (mut repaired, mut rebuilt, mut unsettled_total) = (0, 0, 0);
    for i in 0..steps {
        let idx = Index::open(&dir).unwrap();
        // files other files import, which is where membership moves edges
        let imported: Vec<String> = idx
            .live_files()
            .filter(|(id, _, _)| !idx.in_edges(*id).is_empty())
            .map(|(_, rel, _)| String::from_utf8_lossy(rel).into_owned())
            .collect();
        drop(idx);
        let what = match next(5) {
            0 if !gone.is_empty() => {
                let (rel, body) = gone.swap_remove(next(gone.len()));
                w(&t.root.join(&rel), std::str::from_utf8(&body).unwrap_or(""));
                format!("recreate {rel}")
            }
            1 => {
                let rel = &imported[next(imported.len())];
                let to = format!("{rel}.moved");
                fs::rename(t.root.join(rel), t.root.join(&to)).unwrap();
                gone.push((rel.clone(), fs::read(t.root.join(&to)).unwrap()));
                fs::remove_file(t.root.join(&to)).unwrap();
                format!("rename {rel} away")
            }
            3 => match imported
                .iter()
                .filter_map(|r| module_of_dir(r))
                .nth(next(8))
            {
                Some(rel) if !t.root.join(&rel).exists() => {
                    w(&t.root.join(&rel), "\n");
                    format!("add {rel}")
                }
                _ => continue,
            },
            2 => match imported.iter().filter_map(|r| shadow_of(r)).nth(next(8)) {
                Some(rel) if !t.root.join(&rel).exists() => {
                    w(&t.root.join(&rel), "\n");
                    format!("shadow with {rel}")
                }
                _ => continue,
            },
            _ => {
                let rel = &imported[next(imported.len())];
                gone.push((rel.clone(), fs::read(t.root.join(rel)).unwrap()));
                fs::remove_file(t.root.join(rel)).unwrap();
                format!("delete {rel}")
            }
        };
        let idx = Index::open(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let started = std::time::Instant::now();
        let ch = fresh::check(&idx, &t.root, Mode::Stat, 4).unwrap();
        let check_ms = started.elapsed().as_secs_f64() * 1e3;
        if fresh::rebuild_reason(&idx, &ch).is_some() {
            build(&t.root, &dir, &opts()).unwrap();
            rebuilt += 1;
            eprintln!("step {i}: {what}: rebuilt");
            continue;
        }
        fresh::apply(&idx, &t.root, &ch).unwrap();
        repaired += 1;
        eprintln!(
            "step {i}: {what}: {} files in the delta, check {check_ms:.1} ms",
            ch.count()
        );
        let delta = Index::open(&dir).unwrap();
        let clean_dir = t.base.join(format!("clean-{i}"));
        build(&t.root, &clean_dir, &opts()).unwrap();
        let clean = Index::open(&clean_dir).unwrap();
        let skip = unsettled(&delta, &clean);
        let settled = |e: &(String, String)| !skip.contains(&e.0);
        let d: BTreeSet<_> = edges(&delta).into_iter().filter(settled).collect();
        let c: BTreeSet<_> = edges(&clean).into_iter().filter(settled).collect();
        unsettled_total += skip.len();
        let only_delta: Vec<_> = d.difference(&c).collect();
        let only_clean: Vec<_> = c.difference(&d).collect();
        assert!(
            only_delta.is_empty() && only_clean.is_empty(),
            "step {i}: {what}: only in the delta {only_delta:?}, only in a clean build {only_clean:?}"
        );
        drop(clean);
        fs::remove_dir_all(&clean_dir).unwrap();
    }
    eprintln!(
        "{repaired} deltas compared, {rebuilt} rebuilds, {unsettled_total} importers skipped \
         whose parse fell back in one index only"
    );
    assert!(repaired > 0);
}
