//! The FSEvents check against directory changes a stamp hides (macOS). A
//! separate test binary with one test: it sets the event read's cutoff
//! before any thread starts.
#![cfg(target_os = "macos")]

use greeg_index::Index;
use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Changes, Mode};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct Tmp {
    base: PathBuf,
    root: PathBuf,
    dir: PathBuf,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        // a failed case may leave a directory unreadable
        let _ = fs::set_permissions(self.root.join("locked"), fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// A tree with `lib/thing.py`, in a directory this run creates itself.
fn tree() -> Tmp {
    let base = (0..)
        .map(|n| std::env::temp_dir().join(format!("greeg-fsevents-{}-{n}", std::process::id())))
        .find(|b| match fs::create_dir(b) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => panic!("create {}: {e}", b.display()),
        })
        .unwrap();
    let (root, dir) = (base.join("tree"), base.join("index"));
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(
        root.join("lib/thing.py"),
        "def thing_beta():\n    return 'a'\n",
    )
    .unwrap();
    // files enough that one change stays a delta
    for i in 0..40 {
        fs::write(root.join(format!("f{i:02}.txt")), "hay\n").unwrap();
    }
    Tmp { base, root, dir }
}

fn settle() {
    std::thread::sleep(Duration::from_millis(50));
}

fn build_index(t: &Tmp) {
    let opts = BuildOpts {
        reader_threads: 1,
        quiet: true,
        ..Default::default()
    };
    build(&t.root, &t.dir, &opts).unwrap();
}

/// The FSEvents check. On a machine too loaded for the event read even with
/// a long cutoff the stat pass answers, which this says on stderr; under CI
/// that fails the test.
fn check(t: &Tmp, what: &str) -> Changes {
    let idx = Index::open(&t.dir).unwrap();
    let ch = fresh::check(&idx, &t.root, Mode::FsEvents, 2).unwrap();
    if ch.method != "fsevents" {
        assert!(
            std::env::var_os("CI").is_none(),
            "{what}: {} answered",
            ch.method
        );
        eprintln!("{what}: the stat pass answered ({})", ch.method);
    }
    ch
}

fn rels(paths: impl Iterator<Item = Vec<u8>>) -> Vec<String> {
    let mut v: Vec<String> = paths
        .map(|p| String::from_utf8_lossy(&p).into_owned())
        .collect();
    v.sort();
    v
}

fn touch_as(reference: &Path, target: &Path) {
    let st = std::process::Command::new("touch")
        .arg("-r")
        .arg(reference)
        .arg(target)
        .status()
        .unwrap();
    assert!(st.success());
}

/// A built tree whose `locked/inner.txt` the build could not list; `None`
/// (said on stderr) when this user reads a mode-000 directory anyway (root).
fn locked_tree() -> Option<Tmp> {
    let t = tree();
    let locked = t.root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inner.txt"), "needle\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        eprintln!("skipped: this user reads a mode-000 directory (root)");
        return None;
    }
    settle();
    build_index(&t);
    Some(t)
}

fn unlock(t: &Tmp) {
    fs::set_permissions(t.root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn fsevents_finds_what_a_directory_stamp_hides() {
    // SAFETY: the only test in this binary, and no thread has started yet.
    unsafe { std::env::set_var("GREEG_DEBUG_FSEVENTS_CUTOFF_MS", "15000") };

    // an entry added while the directory's mtime is put back (`rsync -a`)
    let t = tree();
    settle();
    build_index(&t);
    let lib = t.root.join("lib");
    let saved = t.base.join("saved");
    touch_as(&lib, &saved);
    fs::write(lib.join("new.py"), "x = 1\n").unwrap();
    touch_as(&saved, &lib);
    let ch = check(&t, "restored mtime");
    assert_eq!(rels(ch.added.iter().map(|w| w.rel.clone())), ["lib/new.py"]);

    // a directory unreadable at build, readable again
    if let Some(t) = locked_tree() {
        let ch = check(&t, "still unreadable");
        assert!(ch.is_empty(), "an unreadable directory is no change");
        unlock(&t);
        let ch = check(&t, "readable again");
        assert_eq!(
            rels(ch.added.iter().map(|w| w.rel.clone())),
            ["locked/inner.txt"]
        );
    }

    // a directory made unreadable after an edit in it: its files are not
    // deleted. The event log need not name the directory, so that the check
    // also reports itself unreliable is tested on the stat pass (index.rs).
    let t = tree();
    let locked = t.root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inner.txt"), "needle\n").unwrap();
    settle();
    build_index(&t);
    fs::write(locked.join("inner.txt"), "needle 2\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_err() {
        let ch = check(&t, "became unreadable");
        assert!(ch.deleted.is_empty(), "{ch:?}");
    } else {
        eprintln!("skipped: this user reads a mode-000 directory (root)");
    }
    drop(t);

    // readable again with no event to read (a group or a privacy grant): the
    // index is told it has read every event up to now
    if let Some(t) = locked_tree() {
        unlock(&t);
        std::thread::sleep(Duration::from_secs(2));
        let mut m = greeg_index::read_manifest(&t.dir).unwrap();
        m.fsevents_id = greeg_fsevents::current_id();
        greeg_index::write_manifest(&t.dir, &m).unwrap();
        let ch = check(&t, "readable, no event");
        assert_eq!(
            rels(ch.added.iter().map(|w| w.rel.clone())),
            ["locked/inner.txt"]
        );
    }

    // a stored event id the log cannot have given (a restored or copied index
    // directory, a reset event database), or one recorded for another event
    // database: the stat pass answers
    for (what, id, uuid) in [
        ("id ahead of the system's", Some(u64::MAX / 2), None),
        ("no event database recorded", None, Some("")),
        (
            "another event database",
            None,
            Some("00000000-0000-0000-0000-000000000000"),
        ),
    ] {
        let t = tree();
        settle();
        build_index(&t);
        fs::write(lib_of(&t).join("new.py"), "x = 1\n").unwrap();
        let mut m = greeg_index::read_manifest(&t.dir).unwrap();
        if let Some(id) = id {
            m.fsevents_id = id;
        }
        if let Some(uuid) = uuid {
            m.fsevents_uuid = uuid.into();
        }
        greeg_index::write_manifest(&t.dir, &m).unwrap();
        let idx = Index::open(&t.dir).unwrap();
        let ch = fresh::check(&idx, &t.root, Mode::FsEvents, 2).unwrap();
        assert_eq!(ch.method, "stat", "{what}");
        assert_eq!(
            rels(ch.added.iter().map(|w| w.rel.clone())),
            ["lib/new.py"],
            "{what}"
        );
    }

    // a directory listed too soon, named by an event though its stamp did not move
    let t = tree();
    settle();
    fs::File::open(lib_of(&t))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    build_index(&t);
    fs::write(
        lib_of(&t).join("thing.py"),
        "def thing_beta():\n    return 'b'\n",
    )
    .unwrap();
    let ch = check(&t, "listed too soon");
    assert_eq!(rels(ch.touched_dirs.iter().map(|d| d.rel.clone())), ["lib"]);
}

fn lib_of(t: &Tmp) -> PathBuf {
    t.root.join("lib")
}
