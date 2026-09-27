//! Every byte of every component is covered by a check (`integrity.rs`): a
//! changed byte fails `Index::open`, marks the index corrupt when read, or
//! leaves the answers as they were. No panic is caught here.

use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use greeg_index::{Index, plan, read_manifest};
use roaring::RoaringBitmap;
use std::fs;
use std::path::PathBuf;

struct Tmp {
    root: PathBuf,
    dir: PathBuf,
    base: PathBuf,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn tree() -> Tmp {
    // created here, so no file left by another run joins the fixture
    let base = (0..)
        .map(|n| std::env::temp_dir().join(format!("greeg-integrity-{}-{n}", std::process::id())))
        .find(|d| match fs::create_dir(d) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("create {}: {e}", d.display()),
        })
        .unwrap();
    let (root, dir) = (base.join("tree"), base.join("index"));
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        "mod util;\nfn main() {\n    util::helper_alpha();\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/util.rs"),
        "/// helper\npub fn helper_alpha() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    fs::write(root.join("README.md"), "helper_alpha is documented here\n").unwrap();
    Tmp { root, dir, base }
}

/// What a query can learn from the index.
#[derive(Debug, PartialEq)]
struct Answers {
    words: RoaringBitmap,
    grams: RoaringBitmap,
    defs: Vec<(u32, String)>,
    paths: Vec<Vec<u8>>,
    edges: Vec<u32>,
}

fn answers(idx: &Index) -> Answers {
    let q = plan::plan("helper_alp", false, false).unwrap();
    let mut defs: Vec<(u32, String)> = idx
        .lookup("helper_alpha")
        .into_iter()
        .map(|s| (idx.sym_file(s), idx.sym_name(s).to_string()))
        .collect();
    defs.sort();
    let main = idx
        .live_files()
        .find(|(_, r, _)| *r == b"src/main.rs")
        .map(|(id, _, _)| id);
    Answers {
        words: idx.word_candidates(&[b"helper_alpha".to_vec()]),
        grams: idx.candidates(&q),
        defs,
        paths: idx.live_files().map(|(_, r, _)| r.to_vec()).collect(),
        edges: main.map(|m| idx.out_edges(m).to_vec()).unwrap_or_default(),
    }
}

#[test]
fn every_single_byte_flip_is_detected_or_widens() {
    let t = tree();
    // blocks small enough that every component has many, so reads verify
    // some and not others
    let opts = BuildOpts {
        reader_threads: 1,
        quiet: true,
        block: 64,
        ..Default::default()
    };
    build(&t.root, &t.dir, &opts).unwrap();
    // and a delta, so its segment is covered too
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(
        t.root.join("src/util.rs"),
        "pub fn helper_alpha() -> u32 {\n    43\n}\npub fn helper_beta() {}\n",
    )
    .unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::Stat, 1).unwrap();
    assert_eq!(fresh::apply(&idx, &t.root, &ch).unwrap(), 1);
    let want = answers(&Index::open(&t.dir).unwrap());
    assert!(!want.words.is_empty() && !want.defs.is_empty() && !want.edges.is_empty());

    let m = read_manifest(&t.dir).unwrap();
    let names: Vec<&String> = m.roots.keys().collect();
    assert!(names.len() >= 8, "{names:?}");
    let (mut flips, mut failed_open, mut flagged, mut unchanged) = (0, 0, 0, 0);
    for name in names {
        let path = t.dir.join(name);
        let original = fs::read(&path).unwrap();
        for at in 0..original.len() {
            let mut bytes = original.clone();
            bytes[at] ^= 0x01;
            fs::write(&path, &bytes).unwrap();
            flips += 1;
            match Index::open(&t.dir) {
                Err(_) => failed_open += 1,
                Ok(idx) => {
                    let got = answers(&idx);
                    let _ = idx.graph();
                    if idx.corrupt() {
                        flagged += 1;
                    } else {
                        // the flip was in bytes no answer reads, or it is
                        // detected: nothing else may change an answer
                        assert_eq!(got, want, "{name} byte {at} changed an answer unnoticed");
                        unchanged += 1;
                    }
                }
            }
        }
        fs::write(&path, &original).unwrap();
    }
    eprintln!("{flips} flips: {failed_open} failed to open, {flagged} flagged, {unchanged} unread");
    assert!(failed_open > 0 && flagged > 0);
}
