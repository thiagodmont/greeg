//! Kernel micro-benchmarks (PLAN.md M6, CI 5 % gate against `bench/baselines/kernels-<host>.json`).
//! `cargo bench -p greeg-index --bench kernels`, then `bench/bench.py gate kernels`.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use greeg_index::gram::{Dedup, fold_buf, literal_keys};
use greeg_index::plan::plan;
use greeg_lang::{Lang, lexer, sym};
use roaring::RoaringBitmap;

const RUST_SNIPPET: &str = r#"
/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// ```
/// let handle = tokio::spawn(async { 1 + 1 });
/// ```
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // TODO: the "fast path" avoids the allocation when the runtime is idle.
    let id = task::Id::next();
    let name = format!("task-{}", id); /* block comment with 'quotes' */
    context::with_scheduler(|maybe_cx| match maybe_cx {
        Some(cx) => cx.spawn(future, id),
        None => panic!("`spawn` called outside of a Tokio runtime: {}", name),
    })
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.raw.remote_abort();
    }
    pub fn is_finished(&self) -> bool {
        let state = self.raw.header().state.load();
        state.is_complete()
    }
}
"#;

const PY_SNIPPET: &str = r#"
class ModelAdmin(BaseModelAdmin):
    """Encapsulate all admin options and functionality for a given model."""

    list_display = ("__str__",)
    # A comment about the queryset
    def get_queryset(self, request):
        qs = self.model._default_manager.get_queryset()
        ordering = self.get_ordering(request)  # trailing 'comment'
        if ordering:
            qs = qs.order_by(*ordering)
        return qs

    def message_user(self, request, message, level=messages.INFO):
        if not isinstance(level, int):
            raise ValueError("Bad message level string: %s" % level)
        messages.add_message(request, level, message)
"#;

fn repeat_to(snippet: &str, bytes: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes + snippet.len());
    while v.len() < bytes {
        v.extend_from_slice(snippet.as_bytes());
    }
    v
}

fn bench_grams(c: &mut Criterion) {
    let mut g = c.benchmark_group("gram");
    let mut src = repeat_to(RUST_SNIPPET, 1 << 20);
    fold_buf(&mut src);
    g.throughput(Throughput::Bytes(src.len() as u64));
    let mut dedup = Dedup::new();
    let mut out = Vec::new();
    g.bench_function("extract_1MiB", |b| b.iter(|| dedup.extract(black_box(&src), &mut out)));
    g.bench_function("literal_keys", |b| b.iter(|| literal_keys(black_box(b"fn poll_read(&mut self, cx: &mut Context"))));
    g.finish();
}

fn bench_plan(c: &mut Criterion) {
    let pats: [(&str, bool); 8] = [
        ("createSourceFile", false),
        ("Pin<&mut Self>", true),
        (r"\bpoll_\w+\(", false),
        (r"fn \w+_borrowck", false),
        (r"(get|set)_queryset", false),
        (r"impl<[^>]+> \w+ for \w+", false),
        (r"TODO|FIXME|XXX", false),
        (r"[a-z]+", false),
    ];
    let mut g = c.benchmark_group("plan");
    for (p, fixed) in pats {
        g.bench_with_input(BenchmarkId::from_parameter(p), &(p, fixed), |b, (p, fixed)| b.iter(|| plan(black_box(p), *fixed, false).unwrap()));
    }
    g.finish();
}

fn bench_lexer(c: &mut Criterion) {
    let mut g = c.benchmark_group("lexer");
    for (name, lang, snippet) in [("rust", Lang::Rust, RUST_SNIPPET), ("python", Lang::Python, PY_SNIPPET)] {
        let src = repeat_to(snippet, 1 << 20);
        g.throughput(Throughput::Bytes(src.len() as u64));
        g.bench_function(format!("{name}_1MiB"), |b| b.iter(|| lexer::lex(lang, black_box(&src))));
    }
    g.finish();
}

fn bench_extract(c: &mut Criterion) {
    let mut g = c.benchmark_group("extract");
    for (name, lang, snippet) in [("rust", Lang::Rust, RUST_SNIPPET), ("python", Lang::Python, PY_SNIPPET)] {
        let src = repeat_to(snippet, 64 << 10);
        g.throughput(Throughput::Bytes(src.len() as u64));
        g.bench_function(format!("{name}_64KiB"), |b| b.iter(|| sym::extract(lang, false, black_box(&src))));
    }
    g.finish();
}

fn bench_postings(c: &mut Criterion) {
    let mut g = c.benchmark_group("postings");
    let mk = |seed: u32, n: u32, stride: u32| -> RoaringBitmap { (0..n).map(|i| (i * stride + seed) % 200_000).collect() };
    let a = mk(1, 60_000, 3);
    let b = mk(7, 40_000, 5);
    let d = mk(11, 20_000, 7);
    let ser: Vec<u8> = {
        let mut v = Vec::new();
        a.serialize_into(&mut v).unwrap();
        v
    };
    g.bench_function("and3_60k_40k_20k", |bb| bb.iter(|| black_box(&a) & black_box(&b) & black_box(&d)));
    g.bench_function("deserialize_60k", |bb| bb.iter(|| RoaringBitmap::deserialize_from(black_box(&ser[..])).unwrap()));
    g.finish();
}

criterion_group!(kernels, bench_grams, bench_plan, bench_lexer, bench_extract, bench_postings);
criterion_main!(kernels);
