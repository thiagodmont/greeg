//! Language layer v0: language detection, per-file flags, noncode lexing and
//! regex-based definition extraction with enclosing-symbol ranges.
//!
//! Everything here is byte-oriented and allocation-light: it runs only on
//! files that produced at least one hit.

pub mod defs;
pub mod extra;
pub mod lexer;
pub mod sym;

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lang {
    None = 0,
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Kotlin,
    /// Text-like file with no grammar (md, json, yaml, toml, ...): searched, never outlined.
    Text,
    /// Runtime-loaded language (index into `extra::registry()`).
    Extra(u8),
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Lang::None => "none",
            Lang::Python => "python",
            Lang::Rust => "rust",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Kotlin => "kotlin",
            Lang::Text => "text",
            Lang::Extra(i) => extra::get(i).map(|l| l.name).unwrap_or("extra"),
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Lang::None => "-",
            Lang::Python => "py",
            Lang::Rust => "rs",
            Lang::JavaScript => "js",
            Lang::TypeScript => "ts",
            Lang::Kotlin => "kt",
            Lang::Text => "txt",
            Lang::Extra(i) => extra::get(i).map(|l| l.name).unwrap_or("extra"),
        }
    }
    pub fn has_grammar(self) -> bool {
        !matches!(self, Lang::None | Lang::Text)
    }
    /// Indentation-scoped (Python) versus brace-scoped bodies.
    pub fn indent_scoped(self) -> bool {
        matches!(self, Lang::Python)
    }
    /// Stable byte code for the file table (built-ins 0..16, extras 16 + index).
    pub fn code(self) -> u8 {
        match self {
            Lang::None => 0,
            Lang::Python => 1,
            Lang::Rust => 2,
            Lang::JavaScript => 3,
            Lang::TypeScript => 4,
            Lang::Kotlin => 5,
            Lang::Text => 6,
            Lang::Extra(i) => 16u8.saturating_add(i),
        }
    }
    pub fn from_path(path: &Path) -> Lang {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = match name.rsplit_once('.') {
            Some((_, e)) => e,
            None => return Lang::None,
        };
        match ext {
            "py" | "pyi" | "pyw" => Lang::Python,
            "rs" => Lang::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Lang::TypeScript,
            "kt" | "kts" => Lang::Kotlin,
            "md" | "txt" | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "css" | "scss"
            | "sql" | "sh" | "bash" | "zsh" | "ini" | "cfg" | "conf" | "env" | "csv" | "rst"
            | "proto" | "graphql" | "gql" | "tf" | "hcl" | "gradle" | "properties" | "lock" => {
                Lang::Text
            }
            _ => match extra::by_extension(ext) {
                Some(i) => Lang::Extra(i),
                None => Lang::None,
            },
        }
    }
}

/// Per-file flags (see DESIGN.md §8). Path-derived flags are cheap and
/// computed for every file; content-derived flags only for files we read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileFlags(pub u16);

impl FileFlags {
    pub const TEST: u16 = 1 << 0;
    pub const GENERATED: u16 = 1 << 1;
    pub const VENDORED: u16 = 1 << 2;
    pub const MINIFIED: u16 = 1 << 3;
    pub const BINARY: u16 = 1 << 4;
    pub const LOCKFILE: u16 = 1 << 5;
    pub const HUGE: u16 = 1 << 6;
    pub const PARSE_ERRORS: u16 = 1 << 7;
    pub const IGNORED: u16 = 1 << 8;

    pub fn has(self, f: u16) -> bool {
        self.0 & f != 0
    }
    pub fn set(&mut self, f: u16) {
        self.0 |= f;
    }
    pub fn names(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (bit, n) in [
            (Self::TEST, "test"),
            (Self::GENERATED, "generated"),
            (Self::VENDORED, "vendored"),
            (Self::MINIFIED, "minified"),
            (Self::BINARY, "binary"),
            (Self::LOCKFILE, "lockfile"),
            (Self::HUGE, "huge"),
            (Self::PARSE_ERRORS, "parse-errors"),
            (Self::IGNORED, "ignored"),
        ] {
            if self.has(bit) {
                v.push(n);
            }
        }
        v
    }
    /// The demotion class used for ranking and reporting.
    pub fn demoted(self) -> bool {
        self.has(Self::TEST | Self::GENERATED | Self::VENDORED | Self::MINIFIED | Self::LOCKFILE)
    }
}

/// Path-segment rules (DESIGN.md §8). Every list here is mirrored in that
/// table; keep them in sync. Segments are matched case-insensitively against
/// each directory component of the relative path.
const TEST_SEGMENTS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "specs",
    "testing",
    "testdata",
    "test_data",
    "fixtures",
    "snapshots",
    "__snapshots__",
    "e2e",
    "integration-tests",
    "mock",
    "mocks",
    "__mocks__",
    "stub",
    "stubs",
    "fake",
    "fakes",
];
const VENDOR_SEGMENTS: &[&str] = &[
    "vendor",
    "vendored",
    "third_party",
    "thirdparty",
    "third-party",
    "node_modules",
    ".yarn",
    "bower_components",
    "site-packages",
    "_vendor",
];
const GENERATED_SEGMENTS: &[&str] = &[
    "generated",
    "__generated__",
    "_gen",
    "autogen",
    "compiled",
    "dist",
    ".next",
    "target",
];
const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "gradle.lockfile",
    "pixi.lock",
    "composer.lock",
    "Gemfile.lock",
    "bun.lockb",
    "bun.lock",
    "flake.lock",
];

/// `*Test.kt` / `*Tests.kt` with an uppercase `T` boundary: `FooTest.kt` and
/// `FooTests.kt` are tests, `Latest.kt` is not.
fn is_kotlin_test_name(name: &str) -> bool {
    name.ends_with("Test.kt") || name.ends_with("Tests.kt")
}

/// Flags derivable from the relative path alone.
pub fn path_flags(rel: &str) -> FileFlags {
    let mut f = FileFlags::default();
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    };
    let mut spec_dir = false;
    for seg in dir.split('/') {
        let s = seg.to_ascii_lowercase();
        if TEST_SEGMENTS.contains(&s.as_str()) {
            f.set(FileFlags::TEST);
        }
        if s == "spec" {
            spec_dir = true;
        }
        if VENDOR_SEGMENTS.contains(&s.as_str()) {
            f.set(FileFlags::VENDORED);
        }
        if GENERATED_SEGMENTS.contains(&s.as_str()) {
            f.set(FileFlags::GENERATED);
        }
    }
    let lname = name.to_ascii_lowercase();
    if LOCKFILES.iter().any(|l| l.eq_ignore_ascii_case(name)) {
        f.set(FileFlags::LOCKFILE);
    }
    // `spec/` alone is not a signal (API specs, RFCs); it is one when combined
    // with a `*_spec.*` file name (RSpec, ExUnit).
    if lname.starts_with("test_")
        || lname.contains("_test.")
        || lname.contains(".test.")
        || lname.contains(".spec.")
        || (spec_dir && lname.contains("_spec."))
        || is_kotlin_test_name(name)
        || lname == "conftest.py"
    {
        f.set(FileFlags::TEST);
    }
    if lname.contains(".min.") || lname.ends_with(".bundle.js") {
        f.set(FileFlags::MINIFIED);
    }
    if lname.ends_with("_pb2.py")
        || lname.ends_with(".pb.go")
        || lname.ends_with(".g.dart")
        || lname.ends_with(".g.cs")
        || lname.ends_with(".g.ts")
        || lname.ends_with(".designer.cs")
        || lname.ends_with(".generated.ts")
        || lname.ends_with(".d.ts") && dir.contains("generated")
    {
        f.set(FileFlags::GENERATED);
    }
    f
}

/// Flags that need the first bytes of the file. `head` is at most the first 64 KiB.
pub fn content_flags(head: &[u8], total_len: u64) -> FileFlags {
    let mut f = FileFlags::default();
    let probe = &head[..head.len().min(8192)];
    if memchr::memchr(0, probe).is_some() {
        f.set(FileFlags::BINARY);
        return f;
    }
    if total_len > 4 << 20 {
        f.set(FileFlags::HUGE);
    }
    // line statistics on the head
    let mut lines = 0usize;
    let mut max_line = 0usize;
    let mut last = 0usize;
    for i in memchr::memchr_iter(b'\n', head) {
        lines += 1;
        max_line = max_line.max(i - last);
        last = i + 1;
    }
    let tail_len = head.len() - last;
    max_line = max_line.max(tail_len);
    let avg = if lines > 0 {
        head.len() / lines
    } else {
        head.len()
    };
    if (max_line > 1000 && avg > 200) || (lines == 0 && head.len() > 2000) {
        f.set(FileFlags::MINIFIED);
    }
    // bundler output: a `//# sourceMappingURL=` directive with long lines
    if avg > 120 && memchr::memmem::find(head, b"sourceMappingURL=").is_some() {
        f.set(FileFlags::MINIFIED);
    }
    let first2k = &head[..head.len().min(2048)];
    const MARKERS: &[&[u8]] = &[
        b"@generated",
        b"DO NOT EDIT",
        b"Do not edit",
        b"do not edit",
        b"Code generated by",
        b"autogenerated",
        b"auto-generated",
        b"automatically generated",
        b"AUTO-GENERATED",
        b"This file was generated",
        b"This file is generated",
    ];
    for m in MARKERS {
        if memchr::memmem::find(first2k, m).is_some() {
            f.set(FileFlags::GENERATED);
            break;
        }
    }
    f
}

/// Recognized definition kinds, normalized across languages (DESIGN.md §3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    TypeAlias,
    Module,
    Object,
    Impl,
    Constant,
    Variable,
    Macro,
    Field,
    Variant,
}

impl DefKind {
    pub fn name(self) -> &'static str {
        match self {
            DefKind::Function => "fn",
            DefKind::Method => "method",
            DefKind::Class => "class",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            DefKind::Interface => "interface",
            DefKind::TypeAlias => "type",
            DefKind::Module => "mod",
            DefKind::Object => "object",
            DefKind::Impl => "impl",
            DefKind::Constant => "const",
            DefKind::Variable => "var",
            DefKind::Macro => "macro",
            DefKind::Field => "field",
            DefKind::Variant => "variant",
        }
    }
    pub fn is_container(self) -> bool {
        matches!(
            self,
            DefKind::Class
                | DefKind::Struct
                | DefKind::Enum
                | DefKind::Trait
                | DefKind::Interface
                | DefKind::Module
                | DefKind::Object
                | DefKind::Impl
        )
    }
}

/// Does this line start an import statement in the language?
pub fn is_import_line(lang: Lang, line: &[u8]) -> bool {
    let t = trim_start(line);
    let starts = |p: &[u8]| t.starts_with(p);
    match lang {
        Lang::Python => starts(b"import ") || starts(b"from "),
        Lang::Rust => {
            starts(b"use ")
                || starts(b"pub use ")
                || starts(b"pub(crate) use ")
                || starts(b"extern crate ")
        }
        Lang::JavaScript | Lang::TypeScript => {
            starts(b"import ")
                || starts(b"import{")
                || starts(b"export ") && (memchr::memmem::find(t, b" from ").is_some())
                || memchr::memmem::find(t, b"require(").is_some()
                    && (starts(b"const ") || starts(b"let ") || starts(b"var "))
        }
        Lang::Kotlin => starts(b"import "),
        Lang::Extra(i) => extra::get(i)
            .map(|l| l.imports.iter().any(|p| starts(p.as_bytes())))
            .unwrap_or(false),
        _ => false,
    }
}

pub fn trim_start(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    &b[i..]
}

pub fn indent_of(line: &[u8]) -> usize {
    let mut n = 0;
    for &c in line {
        match c {
            b' ' => n += 1,
            b'\t' => n += 4,
            _ => break,
        }
    }
    n
}

/// ripgrep transcodes UTF-16 files that start with a byte-order mark (its
/// default `--encoding auto`); greeg does the same so those files are
/// searched instead of skipped as binary. Returns true when `buf` was rewritten.
pub fn transcode_utf16(buf: &mut Vec<u8>) -> bool {
    let le = buf.starts_with(&[0xFF, 0xFE]);
    let be = buf.starts_with(&[0xFE, 0xFF]);
    if !(le || be) {
        return false;
    }
    let units = buf[2..].chunks_exact(2).map(|c| {
        if le {
            u16::from_le_bytes([c[0], c[1]])
        } else {
            u16::from_be_bytes([c[0], c[1]])
        }
    });
    let mut out = String::with_capacity(buf.len());
    for r in char::decode_utf16(units) {
        out.push(r.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    *buf = out.into_bytes();
    true
}

/// `fs::read` plus [`transcode_utf16`].
pub fn read_text(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    let mut v = std::fs::read(path)?;
    transcode_utf16(&mut v);
    Ok(v)
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    fn has(rel: &str, bit: u16) -> bool {
        path_flags(rel).has(bit)
    }

    #[test]
    fn test_segments_and_names() {
        for p in [
            "src/__mocks__/fs.ts",
            "a/mocks/x.py",
            "a/mock/x.rs",
            "lib/stubs/s.kt",
            "lib/stub/s.kt",
            "x/fakes/f.go",
            "x/fake/f.go",
            "tests/a.rs",
            "src/testing/util.go",
            "pkg/fixtures/a.json",
            "e2e/login.ts",
            "testdata/x.txt",
        ] {
            assert!(has(p, FileFlags::TEST), "{p}");
        }
        for p in [
            "src/a_test.go",
            "src/a_test.c",
            "src/foo_test.py",
            "src/x.test.ts",
            "src/x.spec.ts",
            "spec/models/user_spec.rb",
            "src/FooTest.kt",
            "src/FooTests.kt",
            "src/test_util.py",
            "conftest.py",
        ] {
            assert!(has(p, FileFlags::TEST), "{p}");
        }
        for p in [
            "src/Latest.kt",
            "src/latest.kt",
            "spec/openapi.yaml",
            "spec/rfc.md",
            "src/attest.rs",
            "src/contest.py",
            "specification/x.rs",
        ] {
            assert!(!has(p, FileFlags::TEST), "{p}");
        }
    }

    #[test]
    fn vendored_and_generated_segments() {
        for p in [
            "vendor/x.go",
            "node_modules/a/i.js",
            "third_party/x.c",
            "site-packages/a.py",
            "x/_vendor/y.py",
        ] {
            assert!(has(p, FileFlags::VENDORED), "{p}");
        }
        for p in ["src/external/x.rs", "pkg/deps/y.go", "externals/z.js"] {
            assert!(!has(p, FileFlags::VENDORED), "{p}");
        }
        for p in [
            "generated/a.rs",
            "src/__generated__/b.ts",
            "dist/c.js",
            "target/d.rs",
            "x/autogen/e.py",
            "a.g.dart",
            "a.g.cs",
            "a.g.ts",
            "a_pb2.py",
            "a.pb.go",
            "Form.Designer.cs",
            "api.generated.ts",
        ] {
            assert!(has(p, FileFlags::GENERATED), "{p}");
        }
        for p in [
            "pkg/build/x.go",
            "src/out/y.rs",
            "gen/z.py",
            "a.g.rs",
            "config.g.yaml",
            "src/build.rs",
        ] {
            assert!(!has(p, FileFlags::GENERATED), "{p}");
        }
    }

    #[test]
    fn minified_from_content() {
        let long = "x".repeat(1500);
        let mut bundle = String::new();
        for _ in 0..3 {
            bundle.push_str(&long);
            bundle.push('\n');
        }
        bundle.push_str("//# sourceMappingURL=app.js.map\n");
        assert!(content_flags(bundle.as_bytes(), bundle.len() as u64).has(FileFlags::MINIFIED));
        let mut short = String::new();
        for i in 0..200 {
            short.push_str(&format!("const v{i} = {i};\n"));
        }
        short.push_str("//# sourceMappingURL=app.js.map\n");
        assert!(!content_flags(short.as_bytes(), short.len() as u64).has(FileFlags::MINIFIED));
    }
}

#[cfg(test)]
mod utf16_tests {
    use super::*;
    #[test]
    fn transcodes_bom_files_only() {
        let mut le: Vec<u8> = vec![0xFF, 0xFE];
        for u in "// node\nlet x = 1;\n".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert!(transcode_utf16(&mut le));
        assert_eq!(le, b"// node\nlet x = 1;\n");
        let mut be: Vec<u8> = vec![0xFE, 0xFF];
        for u in "é".encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert!(transcode_utf16(&mut be));
        assert_eq!(be, "é".as_bytes());
        let mut plain = b"fn main() {}".to_vec();
        assert!(!transcode_utf16(&mut plain));
        assert_eq!(plain, b"fn main() {}");
    }
}
