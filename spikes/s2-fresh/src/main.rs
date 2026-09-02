//! S2: freshness. `snapshot ROOT SNAP`, `check ROOT SNAP [--threads N]`, `fsid`, `fssince ID ROOT`.
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone)]
struct FileRec { path: String, size: u64, mtime_ns: i64, ino: u64 }
#[derive(Clone)]
struct DirRec { path: String, entry_hash: u64 }

fn dir_hash(p: &Path) -> u64 {
    let mut names: Vec<Vec<u8>> = match fs::read_dir(p) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().into_encoded_bytes()).collect(),
        Err(_) => return 0,
    };
    names.sort();
    let mut h = 0u64;
    for n in names { h = h.rotate_left(13) ^ xxh3_64(&n); }
    h
}

fn walk(root: &Path) -> (Vec<FileRec>, Vec<DirRec>) {
    let mut files = vec![]; let mut dirs = vec![];
    for e in ignore::WalkBuilder::new(root).hidden(true).build().filter_map(|e| e.ok()) {
        let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().into_owned();
        match e.file_type() {
            Some(t) if t.is_dir() => dirs.push(DirRec { path: rel, entry_hash: dir_hash(e.path()) }),
            Some(t) if t.is_file() => {
                if let Ok(md) = e.metadata() {
                    files.push(FileRec { path: rel, size: md.len(), mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(), ino: md.ino() });
                }
            }
            _ => {}
        }
    }
    (files, dirs)
}

fn save(snap: &Path, files: &[FileRec], dirs: &[DirRec]) -> Result<()> {
    let mut s = String::new();
    for f in files { s += &format!("F\t{}\t{}\t{}\t{}\n", f.path, f.size, f.mtime_ns, f.ino); }
    for d in dirs { s += &format!("D\t{}\t{}\n", d.path, d.entry_hash); }
    fs::write(snap, s)?; Ok(())
}
fn load(snap: &Path) -> Result<(Vec<FileRec>, Vec<DirRec>)> {
    let mut files = vec![]; let mut dirs = vec![];
    for line in fs::read_to_string(snap)?.lines() {
        let p: Vec<&str> = line.split('\t').collect();
        match p[0] {
            "F" => files.push(FileRec { path: p[1].into(), size: p[2].parse()?, mtime_ns: p[3].parse()?, ino: p[4].parse()? }),
            "D" => dirs.push(DirRec { path: p[1].into(), entry_hash: p[2].parse()? }),
            _ => {}
        }
    }
    Ok((files, dirs))
}

fn check(root: &Path, files: &[FileRec], dirs: &[DirRec]) -> (usize, usize, usize, f64, f64) {
    let t = Instant::now();
    let changed: usize = files.par_iter().map(|f| {
        match fs::symlink_metadata(root.join(&f.path)) {
            Ok(md) => (md.len() != f.size || md.mtime() * 1_000_000_000 + md.mtime_nsec() != f.mtime_ns || md.ino() != f.ino) as usize,
            Err(_) => 1,
        }
    }).sum();
    let stat_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let dchanged: usize = dirs.par_iter().map(|d| (dir_hash(&root.join(&d.path)) != d.entry_hash) as usize).sum();
    let dir_ms = t.elapsed().as_secs_f64() * 1e3;
    (changed, dchanged, files.len(), stat_ms, dir_ms)
}

#[cfg(target_os = "macos")]
mod fsev {
    use fsevent_sys::core_foundation as cf;
    use fsevent_sys::*;
    use std::ffi::{c_void, CStr};
    use std::os::raw::c_char;
    use std::time::Instant;
    unsafe extern "C" {
        fn FSEventsGetCurrentEventId() -> FSEventStreamEventId;
        fn FSEventStreamScheduleWithRunLoop(s: FSEventStreamRef, rl: cf::CFRunLoopRef, mode: cf::CFStringRef);
        fn FSEventStreamStart(s: FSEventStreamRef) -> bool;
        fn FSEventStreamStop(s: FSEventStreamRef);
        fn FSEventStreamRelease(s: FSEventStreamRef);
        fn CFRunLoopRunInMode(mode: cf::CFStringRef, seconds: f64, return_after_source_handled: bool) -> i32;
        static kCFRunLoopDefaultMode: cf::CFStringRef;
    }
    struct State { paths: Vec<(String, u32)>, done: bool, dropped: bool }
    extern "C" fn cb(_s: FSEventStreamRef, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const FSEventStreamEventFlags, _ids: *const FSEventStreamEventId) {
        let st = unsafe { &mut *(info as *mut State) };
        let paths = paths as *const *const c_char;
        for i in 0..n {
            let fl = unsafe { *flags.add(i) };
            if fl & kFSEventStreamEventFlagHistoryDone != 0 { st.done = true; continue; }
            if fl & (kFSEventStreamEventFlagKernelDropped | kFSEventStreamEventFlagUserDropped | kFSEventStreamEventFlagEventIdsWrapped) != 0 { st.dropped = true; }
            let p = unsafe { CStr::from_ptr(*paths.add(i)) }.to_string_lossy().into_owned();
            st.paths.push((p, fl));
        }
    }
    pub fn current_id() -> u64 { unsafe { FSEventsGetCurrentEventId() } }
    pub fn since(id: u64, root: &str) -> (Vec<(String, u32)>, bool, bool, f64) {
        let t = Instant::now();
        let mut st = State { paths: vec![], done: false, dropped: false };
        unsafe {
            let mut err = std::ptr::null_mut();
            let cfpath = cf::str_path_to_cfstring_ref(root, &mut err);
            let arr = cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks);
            cf::CFArrayAppendValue(arr, cfpath);
            let ctx = FSEventStreamContext { version: 0, info: &mut st as *mut State as *mut c_void, retain: None, release: None, copy_description: None };
            let stream = FSEventStreamCreate(cf::kCFAllocatorDefault, cb, &ctx, arr, id, 0.0, kFSEventStreamCreateFlagNoDefer);
            FSEventStreamScheduleWithRunLoop(stream, cf::CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            FSEventStreamStart(stream);
            let deadline = Instant::now() + std::time::Duration::from_secs(3);
            while !st.done && Instant::now() < deadline { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, true); }
            FSEventStreamStop(stream); FSEventStreamInvalidate(stream); FSEventStreamRelease(stream);
            cf::CFRelease(arr); cf::CFRelease(cfpath);
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        (st.paths, st.done, st.dropped, ms)
    }
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a[0].as_str() {
        "snapshot" => {
            let t = Instant::now();
            let (f, d) = walk(Path::new(&a[1]));
            let ms = t.elapsed().as_secs_f64() * 1e3;
            save(Path::new(&a[2]), &f, &d)?;
            println!("snapshot files={} dirs={} walk+stat+readdir={:.0}ms", f.len(), d.len(), ms);
        }
        "check" => {
            let threads: usize = a.iter().position(|x| x == "--threads").map(|i| a[i + 1].parse().unwrap()).unwrap_or(4);
            rayon::ThreadPoolBuilder::new().num_threads(threads).build_global()?;
            let root = PathBuf::from(&a[1]);
            let (f, d) = load(Path::new(&a[2]))?;
            for _ in 0..3 {
                let (c, dc, n, sm, dm) = check(&root, &f, &d);
                println!("threads={} files={} dirs={} changed_files={} changed_dirs={} stat={:.1}ms readdir={:.1}ms total={:.1}ms", threads, n, d.len(), c, dc, sm, dm, sm + dm);
            }
        }
        #[cfg(target_os = "macos")]
        "fsid" => println!("{}", fsev::current_id()),
        #[cfg(target_os = "macos")]
        "fssince" => {
            let id: u64 = a[1].parse()?;
            let root = fs::canonicalize(&a[2])?.to_string_lossy().into_owned();
            let (paths, done, dropped, ms) = fsev::since(id, &root);
            let mut dirs: HashMap<String, u32> = HashMap::new();
            for (p, fl) in &paths { *dirs.entry(p.clone()).or_default() |= fl; }
            println!("fssince id={} events={} distinct_dirs={} history_done={} dropped={} time={:.1}ms", id, paths.len(), dirs.len(), done, dropped, ms);
            let mut v: Vec<_> = dirs.into_iter().collect(); v.sort();
            for (p, fl) in v.iter().take(12) { println!("  {:#x} {}", fl, p.strip_prefix(&root).unwrap_or(p)); }
            if v.len() > 12 { println!("  ... {} more", v.len() - 12); }
        }
        _ => anyhow::bail!("usage"),
    }
    Ok(())
}
