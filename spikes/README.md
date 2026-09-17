# M0 spikes

Throwaway measurement binaries used to settle a question and then kept
only for reference. Nothing here is built or tested by CI.

```
cargo build --release
./target/release/s1-grams ROOT --threads 4 --q ident1,ident2
./target/release/s2-fresh snapshot ROOT snap.tsv && ./target/release/s2-fresh check ROOT snap.tsv --threads 4
./target/release/s2-fresh fsid ; ./target/release/s2-fresh fssince ID ROOT   # macOS
./target/release/s3-kotlin ROOT...
```
