//! FSEvents and a directory listed too soon (macOS). A separate test binary:
//! it sets the event read's cutoff before any thread starts.
#![cfg(target_os = "macos")]

use greeg_index::Index;
use greeg_index::build::{BuildOpts, build};
use greeg_index::fresh::{self, Mode};
use std::fs;
use std::time::{Duration, SystemTime};

/// FSEvents names the directory of an edit; a directory listed too soon is
/// listed again then, though its stamp did not move.
#[test]
fn fsevents_lists_again_a_directory_listed_too_soon() {
    // SAFETY: the only test in this binary, and no thread has started yet.
    unsafe { std::env::set_var("GREEG_DEBUG_FSEVENTS_CUTOFF_MS", "15000") };
    let base = std::env::temp_dir().join(format!("greeg-fsevents-recheck-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (root, dir) = (base.join("tree"), base.join("index"));
    let lib = root.join("lib");
    fs::create_dir_all(&lib).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(lib.join("thing.py"), "def thing_beta():\n    return 'a'\n").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    // an mtime ahead of the clock: every listing comes too soon after it
    fs::File::open(&lib)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    let opts = BuildOpts {
        reader_threads: 1,
        quiet: true,
        ..Default::default()
    };
    build(&root, &dir, &opts).unwrap();
    // an in-place edit: an event in `lib`, whose own stamp does not move
    fs::write(lib.join("thing.py"), "def thing_beta():\n    return 'b'\n").unwrap();
    let idx = Index::open(&dir).unwrap();
    let ch = fresh::check(&idx, &root, Mode::FsEvents, 2).unwrap();
    drop(idx);
    let _ = fs::remove_dir_all(&base);
    if ch.method != "fsevents" {
        // a loaded machine: the stat pass answered, which lists it again too
        eprintln!("the event read ran past its cutoff; the stat pass answered");
    }
    let touched: Vec<&[u8]> = ch.touched_dirs.iter().map(|d| &d.rel[..]).collect();
    assert_eq!(touched, [b"lib".as_slice()]);
}
