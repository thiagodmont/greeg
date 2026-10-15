//! A ranked search's memory does not grow with a file's matched lines.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Peak;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        NOW.fetch_sub(layout.size(), Relaxed);
    }
}

#[global_allocator]
static ALLOC: Peak = Peak;

#[test]
fn a_dense_file_is_ranked_in_bounded_memory() {
    let dir = std::env::temp_dir().join(format!("greeg-ranked-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 4 MiB, every line a match
    let lines = (2 << 20) - 1;
    std::fs::write(dir.join("dense.rs"), "a\n".repeat(lines)).unwrap();
    let o = greeg_query::Options {
        pattern: "a".into(),
        root: dir.clone(),
        threads: 1,
        keep_lines: greeg_query::RANKED_LINES,
        ..Default::default()
    };
    let before = NOW.load(Relaxed);
    PEAK.store(before, Relaxed);
    let r = greeg_query::scan(&o).unwrap();
    let grew = PEAK.load(Relaxed) - before;
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(r.stats.total_hits, lines);
    let f = &r.files[0];
    let dropped = f.dropped.as_deref().unwrap().lines as usize;
    assert_eq!(f.hits.len() + dropped, lines);
    assert!(grew < 16 << 20, "peak grew by {grew} bytes");
}
