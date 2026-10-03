//! Work over a list on several threads, handing the results over in list
//! order with a bounded number held: what a streamed answer needs.

use anyhow::Result;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Condvar, Mutex};

type Outcome<R> = std::thread::Result<Vec<Option<R>>>;

/// Items a worker claims at once: one lock and one wake per chunk, not per item.
pub(crate) const CHUNK: usize = 32;

struct State<R> {
    /// Chunks claimed, and the first not yet emitted.
    claimed: usize,
    emitted: usize,
    done: BTreeMap<usize, Outcome<R>>,
    /// The number of chunks, once the items ran out.
    total: Option<usize>,
    stop: bool,
}

/// Run `work` on each item `items` yields, with `threads` workers (each with
/// its own `init()` state), and pass the results to `emit` in item order, on
/// the calling thread. Workers take chunks of items as they come, so a slow
/// source (a walk) overlaps the work, and stay about `window` items (at least
/// a chunk per worker) ahead of the next one to emit. An error from `emit`
/// stops the workers and is returned; a panic in `work` is resumed here.
pub(crate) fn run<T: Send, S, R: Send>(
    items: impl Iterator<Item = T> + Send,
    threads: usize,
    window: usize,
    init: impl Fn() -> S + Sync,
    work: impl Fn(&mut S, T) -> Option<R> + Sync,
    emit: &mut dyn FnMut(R) -> Result<()>,
) -> Result<()> {
    let threads = threads.max(1);
    // every worker can hold a chunk while the writer waits for another
    let ahead = window.div_ceil(CHUNK).max(threads + 1);
    // the source, and the id of the chunk it gives next
    let source = Mutex::new((items.fuse(), 0usize));
    let state = Mutex::new(State {
        claimed: 0,
        emitted: 0,
        done: BTreeMap::new(),
        total: None,
        stop: false,
    });
    // the writer waits for its next chunk; workers wait for room
    let (ready, room) = (Condvar::new(), Condvar::new());
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut s = init();
                loop {
                    {
                        let mut st = state.lock().unwrap();
                        while !st.stop && st.total.is_none() && st.claimed >= st.emitted + ahead {
                            st = room.wait(st).unwrap();
                        }
                        if st.stop || st.total.is_some() {
                            return;
                        }
                        st.claimed += 1;
                    }
                    let (c, part) = {
                        let mut src = source.lock().unwrap();
                        let part: Vec<T> = src.0.by_ref().take(CHUNK).collect();
                        src.1 += usize::from(!part.is_empty());
                        (src.1.wrapping_sub(1), part)
                    };
                    let mut st = state.lock().unwrap();
                    if part.is_empty() {
                        let n = source.lock().unwrap().1;
                        st.total = Some(n);
                        ready.notify_one();
                        room.notify_all();
                        return;
                    }
                    drop(st);
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        part.into_iter()
                            .map(|t| work(&mut s, t))
                            .collect::<Vec<_>>()
                    }));
                    let failed = r.is_err();
                    st = state.lock().unwrap();
                    st.done.insert(c, r);
                    st.stop |= failed;
                    if c == st.emitted || failed {
                        ready.notify_one();
                    }
                }
            });
        }
        let mut result = Ok(());
        let mut c = 0;
        'chunks: loop {
            let r = {
                let mut st = state.lock().unwrap();
                loop {
                    if let Some(r) = st.done.remove(&c) {
                        break r;
                    }
                    if st.total.is_some_and(|n| c >= n) {
                        break 'chunks;
                    }
                    st = ready.wait(st).unwrap();
                }
            };
            match r {
                Ok(rs) => {
                    for r in rs.into_iter().flatten() {
                        if let Err(e) = emit(r) {
                            result = Err(e);
                            break 'chunks;
                        }
                    }
                }
                Err(panic) => {
                    state.lock().unwrap().stop = true;
                    room.notify_all();
                    resume_unwind(panic);
                }
            }
            c += 1;
            state.lock().unwrap().emitted = c;
            room.notify_all();
        }
        state.lock().unwrap().stop = true;
        room.notify_all();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    #[test]
    fn results_arrive_in_order_with_a_bounded_lead() {
        let items: Vec<usize> = (0..500).collect();
        let (started, emitted) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let lead = AtomicUsize::new(0);
        let mut got = Vec::new();
        run(
            items.iter().copied(),
            8,
            16,
            || (),
            |_, i| {
                started.fetch_add(1, Relaxed);
                lead.fetch_max(i - emitted.load(Relaxed), Relaxed);
                (i % 3 != 0).then_some(i * 2)
            },
            &mut |r| {
                emitted.store(r / 2 + 1, Relaxed);
                got.push(r);
                Ok(())
            },
        )
        .unwrap();
        let want: Vec<usize> = items
            .iter()
            .filter(|i| *i % 3 != 0)
            .map(|i| i * 2)
            .collect();
        assert_eq!(got, want);
        assert_eq!(started.load(Relaxed), 500);
        // chunks of CHUNK items, at least one per worker beyond the one written
        let bound = (16usize.div_ceil(CHUNK).max(9) + 1) * CHUNK;
        assert!(lead.load(Relaxed) < bound, "{}", lead.load(Relaxed));
    }

    #[test]
    fn an_emit_error_stops_the_workers() {
        let items: Vec<usize> = (0..10_000).collect();
        let started = AtomicUsize::new(0);
        let r = run(
            items.iter().copied(),
            4,
            8,
            || (),
            |_, i| {
                started.fetch_add(1, Relaxed);
                Some(i)
            },
            &mut |i| {
                if i == 20 {
                    anyhow::bail!("stop")
                }
                Ok(())
            },
        );
        assert!(r.is_err());
        assert!(
            started.load(Relaxed) <= 7 * CHUNK,
            "{}",
            started.load(Relaxed)
        );
    }

    #[test]
    fn a_panic_in_work_reaches_the_caller() {
        let items: Vec<usize> = (0..100).collect();
        let r = catch_unwind(AssertUnwindSafe(|| {
            run(
                items.iter().copied(),
                4,
                8,
                || (),
                |_, i| {
                    assert!(i != 37, "boom");
                    Some(i)
                },
                &mut |_| Ok(()),
            )
        }));
        assert!(r.is_err());
    }
}
