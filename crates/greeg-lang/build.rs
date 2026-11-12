//! Records the tree-sitter runtime and grammar versions of the workspace lock
//! file as `GREEG_PARSERS` (`name@version`, space-separated): they decide the
//! parse trees, so the index build key hashes them.
use std::path::Path;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let lock = Path::new(&dir).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("{}: {e}; the parser versions come from it", lock.display()));
    let mut parsers = Vec::new();
    let mut name = None;
    for line in text.lines() {
        if let Some(n) = quoted(line, "name = ") {
            name = Some(n);
        } else if let Some(v) = quoted(line, "version = ")
            && let Some(n) = name.take()
            && n.starts_with("tree-sitter")
        {
            parsers.push(format!("{n}@{v}"));
        }
    }
    assert!(
        parsers.iter().any(|p| p.starts_with("tree-sitter@")),
        "no tree-sitter package in {}",
        lock.display()
    );
    println!("cargo:rustc-env=GREEG_PARSERS={}", parsers.join(" "));
}

fn quoted<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?.strip_prefix('"')?.strip_suffix('"')
}
