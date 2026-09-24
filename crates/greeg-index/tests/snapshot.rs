//! Snapshots: a reader uses the whole snapshot the manifest it read names,
//! whatever writers publish or clean up meanwhile, and cleanup removes what
//! no reader can need.

use greeg_index::build::{BuildOpts, after_phase1_on_this_thread, build};
use greeg_index::fresh::{self, Mode};
use greeg_index::snapshot::{MAX_REMOVALS, OPEN_ATTEMPTS, RETIRED_BUILDS, expire_retired, gen_dir};
use greeg_index::{Index, format, read_manifest, write_manifest};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

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

static N: AtomicU32 = AtomicU32::new(0);

/// Two Rust files, one importing the other, so the graph has an edge.
fn tree() -> Tmp {
    let base = (0..)
        .map(|_| {
            std::env::temp_dir().join(format!(
                "greeg-snapshot-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ))
        })
        .find(|d| fs::create_dir(d).is_ok())
        .unwrap();
    let root = base.join("tree");
    let dir = base.join("index");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        "mod util;\nfn main() {\n    util::helper_alpha();\n}\n",
    )
    .unwrap();
    fs::write(root.join("src/util.rs"), "pub fn helper_alpha() {}\n").unwrap();
    Tmp { root, dir, base }
}

fn opts() -> BuildOpts {
    BuildOpts {
        reader_threads: 1,
        quiet: true,
        phase1_only: false,
        ..Default::default()
    }
}

fn id_of(idx: &Index, rel: &str) -> u32 {
    idx.live_files()
        .find(|(_, r, _)| *r == rel.as_bytes())
        .map(|(id, _, _)| id)
        .unwrap_or_else(|| panic!("{rel} not live"))
}

/// Edit `src/util.rs` and publish the change as a delta.
fn edit_and_apply(t: &Tmp) {
    std::thread::sleep(Duration::from_millis(30));
    fs::write(
        t.root.join("src/util.rs"),
        "pub fn helper_alpha() {}\npub fn helper_omega() {}\n",
    )
    .unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::Stat, 1).unwrap();
    assert_eq!(fresh::apply(&idx, &t.root, &ch).unwrap(), 1);
}

fn set_age(p: &Path, age: Duration) {
    let t = SystemTime::now() - age;
    fs::File::open(p).unwrap().set_modified(t).unwrap();
}

#[test]
fn a_reader_opened_before_a_rebuild_keeps_a_whole_generation() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    edit_and_apply(&t);
    let idx = Index::open(&t.dir).unwrap();
    assert_eq!(idx.deltas.len(), 1);
    let old = gen_dir(idx.manifest.epoch);
    // a rebuild, and its cleanup once the grace period is over
    build(&t.root, &t.dir, &opts()).unwrap();
    assert!(expire_retired(&t.dir) > 0);
    assert!(!t.dir.join(&old).exists());
    // the reader still has its base, its delta and its graph
    assert_eq!(idx.lookup("helper_omega").len(), 1, "delta symbols");
    assert_eq!(idx.live_count(), 2);
    let main = id_of(&idx, "src/main.rs");
    assert!(!idx.out_edges(main).is_empty(), "graph edges");
}

#[test]
fn a_lazily_used_graph_survives_cleanup() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let idx = Index::open(&t.dir).unwrap();
    let graph = t
        .dir
        .join(idx.manifest.component(format::COMP_GRAPH).unwrap().0);
    build(&t.root, &t.dir, &opts()).unwrap();
    expire_retired(&t.dir);
    assert!(!graph.exists());
    // first use of the graph, after its file is gone
    let g = idx.graph().expect("graph kept by the open index");
    assert!(!g.out_to.is_empty());
}

#[test]
fn deltas_of_a_previous_generation_are_never_applied() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    edit_and_apply(&t);
    let a = read_manifest(&t.dir).unwrap();
    // the same tree again: a base of the same size, where a delta's first
    // id matches too
    build(&t.root, &t.dir, &opts()).unwrap();
    let mut b = read_manifest(&t.dir).unwrap();
    fs::copy(t.dir.join(a.delta(1)), t.dir.join(b.delta(1))).unwrap();
    b.deltas = 1;
    write_manifest(&t.dir, &b).unwrap();
    let err = Index::open(&t.dir).err().expect("a delta of another build");
    assert!(format!("{err:#}").contains("snapshot"), "{err:#}");
}

#[test]
fn phase_two_publication_does_not_change_an_open_file_table() {
    let t = tree();
    let dir = t.dir.clone();
    let reader = std::sync::Arc::new(std::sync::Mutex::new(None));
    let r = reader.clone();
    // a reader reads the phase-1 manifest, and opens its components only
    // once phase 2 has published
    after_phase1_on_this_thread(Some(Box::new(move || {
        let dir = dir.clone();
        *r.lock().unwrap() = Some(std::thread::spawn(move || {
            Index::open_with(&dir, &mut || {
                let deadline = Instant::now() + Duration::from_secs(20);
                while read_manifest(&dir).is_none_or(|m| !m.phase2) {
                    assert!(Instant::now() < deadline, "phase 2 never published");
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        }));
    })));
    let built = build(&t.root, &t.dir, &opts());
    after_phase1_on_this_thread(None);
    let m = built.unwrap();
    assert!(m.phase2);
    let idx = reader
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .join()
        .unwrap()
        .unwrap();
    assert!(!idx.manifest.phase2);
    // phase 1's file table: no ranks yet, whatever phase 2 wrote since
    assert!(idx.live_files().all(|(_, _, r)| r.rank == 0));
    let now = Index::open(&t.dir).unwrap();
    assert!(now.live_files().any(|(_, _, r)| r.rank != 0));
}

#[test]
fn open_retries_a_bounded_number_of_times_then_fails() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    // every attempt races a rebuild whose cleanup removes what it read
    let mut calls = 0;
    let r = Index::open_with(&t.dir, &mut || {
        calls += 1;
        build(&t.root, &t.dir, &opts()).unwrap();
        expire_retired(&t.dir);
    });
    assert!(r.is_err());
    assert_eq!(calls, OPEN_ATTEMPTS);
    // one race, then the snapshot holds still
    let mut calls = 0;
    let r = Index::open_with(&t.dir, &mut || {
        calls += 1;
        if calls == 1 {
            build(&t.root, &t.dir, &opts()).unwrap();
            expire_retired(&t.dir);
        }
    });
    assert!(r.is_ok());
    assert_eq!(calls, 2);
    // a damaged component of a snapshot that did not change is not retried
    let m = read_manifest(&t.dir).unwrap();
    let grams = t.dir.join(m.component(format::COMP_GRAMS).unwrap().0);
    let bytes = fs::read(&grams).unwrap();
    fs::write(&grams, &bytes[..bytes.len() - 1]).unwrap();
    let mut calls = 0;
    assert!(Index::open_with(&t.dir, &mut || calls += 1).is_err());
    assert_eq!(calls, 1);
}

#[test]
fn orphaned_spill_directories_are_evicted() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let old = Duration::from_secs(11 * 60);
    let gone_pid = {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    };
    let dead = t.dir.join(format!("scratch-{gone_pid}"));
    let live = t
        .dir
        .join(format!("scratch-{}", std::os::unix::process::parent_id()));
    let orphan = t.dir.join("g-00000000000000aa");
    let young = t.dir.join("g-00000000000000bb");
    let tmp = t.dir.join("manifest.1.2.tmp");
    let previous = t.dir.join(gen_dir(read_manifest(&t.dir).unwrap().epoch));
    for d in [&dead, &live, &orphan, &young] {
        fs::create_dir(d).unwrap();
        fs::write(d.join("part"), "x").unwrap();
    }
    fs::write(&tmp, "x").unwrap();
    for p in [&dead, &live, &orphan, &tmp] {
        set_age(p, old);
    }
    // a rebuild cleans up
    build(&t.root, &t.dir, &opts()).unwrap();
    for p in [&dead, &orphan, &tmp] {
        assert!(!p.exists(), "{} kept", p.display());
    }
    // a live process's scratch, a young unnamed build and the previous
    // build (retired, in its grace period) stay
    for p in [&live, &young, &previous] {
        assert!(p.exists(), "{} removed", p.display());
    }
}

#[test]
fn cleanup_removes_a_bounded_number_of_entries_per_pass() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let extra = 6;
    for i in 0..MAX_REMOVALS + extra {
        let d = t.dir.join(format!("g-{:016x}", 0xa000 + i));
        fs::create_dir(&d).unwrap();
        set_age(&d, Duration::from_secs(11 * 60));
    }
    build(&t.root, &t.dir, &opts()).unwrap();
    let left = fs::read_dir(&t.dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("g-000000000000a")
        })
        .count();
    assert_eq!(left, extra);
}

#[test]
fn an_idle_index_drops_retired_entries_on_its_next_check() {
    let t = tree();
    build(&t.root, &t.dir, &opts()).unwrap();
    let previous = t.dir.join(gen_dir(read_manifest(&t.dir).unwrap().epoch));
    build(&t.root, &t.dir, &opts()).unwrap();
    // the grace period, and the TTL stamp, are long past
    let mut m = read_manifest(&t.dir).unwrap();
    assert!(!m.retired.is_empty());
    for r in &mut m.retired {
        r.since_ms = 0;
    }
    m.verified_unix_ms = 0;
    write_manifest(&t.dir, &m).unwrap();
    // a query finds nothing changed
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::Stat, 1).unwrap();
    assert!(ch.is_empty());
    assert_eq!(fresh::apply(&idx, &t.root, &ch).unwrap(), 0);
    assert!(!previous.exists());
    assert!(read_manifest(&t.dir).unwrap().retired.is_empty());
    assert_eq!(idx.live_count(), 2);
}

#[test]
fn a_burst_of_rebuilds_keeps_a_bounded_number_of_builds() {
    let t = tree();
    for _ in 0..6 {
        build(&t.root, &t.dir, &opts()).unwrap();
    }
    let builds = fs::read_dir(&t.dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("g-"))
        .count();
    assert_eq!(builds, 1 + RETIRED_BUILDS);
    assert!(Index::open(&t.dir).is_ok());
}
