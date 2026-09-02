//! Import resolution at index time (DESIGN.md §7.2): map an import statement
//! of a file to the file ids it refers to. Purely path-based for Python,
//! Rust and JS/TS; package-table based for Kotlin. Unresolved imports keep
//! their raw text in `spans.bin` and produce no edge.

use greeg_lang::Lang;
use greeg_lang::sym::Import;
use hashbrown::HashMap;
use std::path::Path;

#[derive(Default, Clone, Debug)]
pub struct FileCtx {
    pub rust_src: String,
    pub rust_mod_dir: String,
}

pub struct Resolver<'a> {
    paths: HashMap<&'a str, u32>,
    /// Python search roots (relative dirs, "" = repo root), longest first.
    py_roots: Vec<String>,
    /// Rust crates: (dir, package name with `-` → `_`, src dir), longest dir first.
    rust_crates: Vec<(String, String, String)>,
    rust_by_name: HashMap<String, usize>,
    rust_by_dir: HashMap<String, usize>,
    /// Kotlin `package` → file ids, and `package.Name` → file id.
    kt_packages: HashMap<String, Vec<u32>>,
    kt_syms: HashMap<String, u32>,
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(dir: &str) -> &str {
    dir.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Normalize `a/./b/../c` → `a/c` (relative to repo root; cannot escape it).
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

impl<'a> Resolver<'a> {
    /// `files`: (id, relative path). `kt`: (id, package, top-level exported names) for Kotlin files.
    pub fn new(
        root: &Path,
        files: &[(u32, &'a str)],
        kt: impl Iterator<Item = (u32, &'a str, Vec<&'a str>)>,
    ) -> Self {
        let mut paths: HashMap<&'a str, u32> = HashMap::with_capacity(files.len());
        let mut py_roots: Vec<String> = vec![String::new()];
        let mut rust_crates: Vec<(String, String, String)> = Vec::new();
        for &(id, rel) in files {
            paths.insert(rel, id);
            let name = rel.rsplit('/').next().unwrap_or(rel);
            match name {
                "pyproject.toml" | "setup.py" | "setup.cfg" => {
                    let d = dir_of(rel).to_string();
                    if !py_roots.contains(&d) {
                        py_roots.push(d.clone());
                    }
                    let src = join(&d, "src");
                    if !py_roots.contains(&src) {
                        py_roots.push(src);
                    }
                }
                "Cargo.toml" => {
                    let d = dir_of(rel).to_string();
                    let pkg = cargo_package_name(&root.join(rel))
                        .unwrap_or_else(|| d.rsplit('/').next().unwrap_or("").to_string())
                        .replace('-', "_");
                    let src = join(&d, "src");
                    rust_crates.push((d, pkg, src));
                }
                _ => {}
            }
        }
        py_roots.sort_by_key(|b| std::cmp::Reverse(b.len()));
        rust_crates.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let rust_by_name = rust_crates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.1.clone(), i))
            .collect();
        let rust_by_dir = rust_crates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.0.clone(), i))
            .collect();
        let mut kt_packages: HashMap<String, Vec<u32>> = HashMap::new();
        let mut kt_syms: HashMap<String, u32> = HashMap::new();
        for (id, pkg, names) in kt {
            kt_packages.entry(pkg.to_string()).or_default().push(id);
            for n in names {
                kt_syms.entry(format!("{pkg}.{n}")).or_insert(id);
            }
        }
        Resolver {
            paths,
            py_roots,
            rust_crates,
            rust_by_name,
            rust_by_dir,
            kt_packages,
            kt_syms,
        }
    }

    /// Per-file context computed once (crate root and module directory for Rust).
    pub fn file_ctx(&self, lang: Lang, from_rel: &str) -> FileCtx {
        let mut ctx = FileCtx::default();
        if lang == Lang::Rust {
            // nearest ancestor directory with a Cargo.toml
            let mut d = dir_of(from_rel);
            let mut found: Option<usize> = None;
            loop {
                if let Some(&i) = self.rust_by_dir.get(d) {
                    found = Some(i);
                    break;
                }
                if d.is_empty() {
                    break;
                }
                d = parent(d);
            }
            ctx.rust_src = found
                .map(|i| self.rust_crates[i].2.clone())
                .unwrap_or_else(|| "src".to_string());
            let fname = from_rel.rsplit('/').next().unwrap_or("");
            let fdir = dir_of(from_rel);
            ctx.rust_mod_dir = if matches!(fname, "mod.rs" | "lib.rs" | "main.rs") {
                fdir.to_string()
            } else {
                join(fdir, fname.trim_end_matches(".rs"))
            };
        }
        ctx
    }

    fn get(&self, p: &str) -> Option<u32> {
        self.paths.get(p).copied()
    }

    /// File ids an import refers to (usually one; several for wildcard Kotlin imports).
    pub fn resolve(&self, lang: Lang, from_rel: &str, im: &Import) -> Vec<u32> {
        let ctx = self.file_ctx(lang, from_rel);
        self.resolve_with(lang, from_rel, &ctx, im)
    }

    pub fn resolve_with(&self, lang: Lang, from_rel: &str, ctx: &FileCtx, im: &Import) -> Vec<u32> {
        let from_id = self.get(from_rel);
        let mut out = match lang {
            Lang::Python => self.python(from_rel, &im.module).into_iter().collect(),
            Lang::Rust => self.rust(ctx, &im.module).into_iter().collect(),
            Lang::JavaScript | Lang::TypeScript => {
                self.js(from_rel, &im.module).into_iter().collect()
            }
            Lang::Kotlin => self.kotlin(&im.module, im.wildcard),
            _ => Vec::new(),
        };
        out.retain(|id| Some(*id) != from_id);
        out
    }

    /// Files sharing a Kotlin package with `pkg`, excluding `self_id` (capped).
    pub fn kotlin_package_peers(&self, pkg: &str, self_id: u32) -> Vec<u32> {
        match self.kt_packages.get(pkg) {
            Some(v) if v.len() <= 30 => v.iter().copied().filter(|&i| i != self_id).collect(),
            _ => Vec::new(),
        }
    }

    fn python(&self, from_rel: &str, module: &str) -> Option<u32> {
        let dots = module.bytes().take_while(|&b| b == b'.').count();
        let rest: Vec<&str> = module[dots..]
            .split('.')
            .filter(|s| !s.is_empty())
            .collect();
        let try_under = |base: &str, segs: &[&str]| -> Option<u32> {
            // longest module path first, then shorter (the tail may be a symbol name)
            for k in (0..=segs.len()).rev() {
                let mut p = base.to_string();
                for s in &segs[..k] {
                    p = join(&p, s);
                }
                if k > 0
                    && let Some(id) = self.get(&format!("{p}.py"))
                {
                    return Some(id);
                }
                if let Some(id) = self.get(&join(&p, "__init__.py")) {
                    return Some(id);
                }
                if k == 0 {
                    break;
                }
            }
            None
        };
        if dots > 0 {
            let mut base = dir_of(from_rel).to_string();
            for _ in 1..dots {
                base = parent(&base).to_string();
            }
            return try_under(&base, &rest);
        }
        // absolute: prefer roots that contain the importing file
        for root in &self.py_roots {
            if (root.is_empty() || from_rel.starts_with(&format!("{root}/")))
                && let Some(id) = try_under(root, &rest)
            {
                return Some(id);
            }
        }
        for root in &self.py_roots {
            if let Some(id) = try_under(root, &rest) {
                return Some(id);
            }
        }
        None
    }

    fn rust(&self, ctx: &FileCtx, module: &str) -> Option<u32> {
        let segs: Vec<&str> = module.split("::").filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return None;
        }
        let src_dir = ctx.rust_src.as_str();
        let mod_dir = ctx.rust_mod_dir.as_str();
        let mut min_k = 0;
        let (base, rest): (String, &[&str]) = match segs[0] {
            "crate" => (src_dir.to_string(), &segs[1..]),
            "self" => (mod_dir.to_string(), &segs[1..]),
            "super" => {
                let mut d = parent(mod_dir).to_string();
                let mut i = 1;
                while i < segs.len() && segs[i] == "super" {
                    d = parent(&d).to_string();
                    i += 1;
                }
                (d, &segs[i..])
            }
            name => match self.rust_by_name.get(name) {
                Some(&ci) => (self.rust_crates[ci].2.clone(), &segs[1..]),
                None => {
                    // 2015-style `use foo::bar` for a sibling module: only accept a real module file
                    min_k = 1;
                    (src_dir.to_string(), &segs[..])
                }
            },
        };
        let mut p = String::with_capacity(base.len() + 64);
        for k in (min_k..=rest.len()).rev() {
            p.clear();
            p.push_str(&base);
            for s in &rest[..k] {
                if !p.is_empty() {
                    p.push('/');
                }
                p.push_str(s);
            }
            let plen = p.len();
            if k > 0 {
                p.push_str(".rs");
                if let Some(id) = self.get(&p) {
                    return Some(id);
                }
                p.truncate(plen);
                p.push_str("/mod.rs");
                if let Some(id) = self.get(&p) {
                    return Some(id);
                }
            } else {
                for f in ["lib.rs", "main.rs", "mod.rs"] {
                    p.truncate(plen);
                    if !p.is_empty() {
                        p.push('/');
                    }
                    p.push_str(f);
                    if let Some(id) = self.get(&p) {
                        return Some(id);
                    }
                }
                p.truncate(plen);
                p.push_str(".rs");
                if let Some(id) = self.get(&p) {
                    return Some(id);
                }
            }
        }
        None
    }

    fn js(&self, from_rel: &str, module: &str) -> Option<u32> {
        if !(module.starts_with("./")
            || module.starts_with("../")
            || module == "."
            || module == "..")
        {
            return None;
        }
        let p = normalize(&join(dir_of(from_rel), module));
        if let Some(id) = self.get(&p) {
            return Some(id);
        }
        let stem = if let Some(s) = p
            .strip_suffix(".js")
            .or_else(|| p.strip_suffix(".jsx"))
            .or_else(|| p.strip_suffix(".mjs"))
            .or_else(|| p.strip_suffix(".cjs"))
        {
            s.to_string()
        } else {
            p.clone()
        };
        for ext in [
            ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
        ] {
            if let Some(id) = self.get(&format!("{stem}{ext}")) {
                return Some(id);
            }
        }
        for idx in [
            "index.ts",
            "index.tsx",
            "index.d.ts",
            "index.js",
            "index.jsx",
            "index.mjs",
        ] {
            if let Some(id) = self.get(&join(&p, idx)) {
                return Some(id);
            }
        }
        None
    }

    fn kotlin(&self, module: &str, wildcard: bool) -> Vec<u32> {
        if wildcard {
            return self
                .kt_packages
                .get(module)
                .map(|v| v.iter().copied().take(20).collect())
                .unwrap_or_default();
        }
        if let Some(&id) = self.kt_syms.get(module) {
            return vec![id];
        }
        // `import a.b.C.Nested` or `import a.b.C.member`: strip trailing segments
        let mut m = module;
        while let Some((head, _)) = m.rsplit_once('.') {
            if let Some(&id) = self.kt_syms.get(head) {
                return vec![id];
            }
            m = head;
        }
        Vec::new()
    }
}

/// `[package] name = "x"` from a Cargo.toml (tiny hand parser; no toml dependency).
fn cargo_package_name(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let mut in_pkg = false;
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_pkg = t == "[package]";
            continue;
        }
        if in_pkg && let Some(rest) = t.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imp(m: &str, wildcard: bool) -> Import {
        Import {
            start: 0,
            end: 0,
            module: m.to_string(),
            names: vec![],
            wildcard,
        }
    }

    #[test]
    fn python_paths() {
        let files: Vec<(u32, &str)> = vec![
            (0, "pkg/__init__.py"),
            (1, "pkg/a/b.py"),
            (2, "pkg/a/__init__.py"),
            (3, "pyproject.toml"),
            (4, "tests/test_x.py"),
        ];
        let r = Resolver::new(Path::new("/nonexistent"), &files, std::iter::empty());
        assert_eq!(
            r.resolve(Lang::Python, "tests/test_x.py", &imp("pkg.a.b", false)),
            vec![1]
        );
        assert_eq!(
            r.resolve(Lang::Python, "tests/test_x.py", &imp("pkg.a", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(Lang::Python, "pkg/a/b.py", &imp(".", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(Lang::Python, "pkg/a/b.py", &imp("..", false)),
            vec![0]
        );
        assert_eq!(
            r.resolve(Lang::Python, "pkg/a/b.py", &imp("pkg.a.b.Thing", false)),
            Vec::<u32>::new()
        ); // self excluded
        assert_eq!(
            r.resolve(Lang::Python, "tests/test_x.py", &imp("os.path", false)),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn rust_paths() {
        let files: Vec<(u32, &str)> = vec![
            (0, "Cargo.toml"),
            (1, "src/lib.rs"),
            (2, "src/a/mod.rs"),
            (3, "src/a/b.rs"),
            (4, "src/c.rs"),
            (5, "crates/x/Cargo.toml"),
            (6, "crates/x/src/lib.rs"),
        ];
        let r = Resolver::new(Path::new("/nonexistent"), &files, std::iter::empty());
        assert_eq!(
            r.resolve(Lang::Rust, "src/c.rs", &imp("crate::a::b::Thing", false)),
            vec![3]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/c.rs", &imp("crate::a", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/a/b.rs", &imp("super::Thing", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/a/mod.rs", &imp("self::b", false)),
            vec![3]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/a/mod.rs", &imp("super::c::X", false)),
            vec![4]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/c.rs", &imp("x::Foo", false)),
            vec![6]
        );
        assert_eq!(
            r.resolve(Lang::Rust, "src/c.rs", &imp("std::io", false)),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn js_paths() {
        let files: Vec<(u32, &str)> = vec![
            (0, "src/a.ts"),
            (1, "src/b/index.ts"),
            (2, "src/c.tsx"),
            (3, "src/d.js"),
        ];
        let r = Resolver::new(Path::new("/nonexistent"), &files, std::iter::empty());
        assert_eq!(
            r.resolve(Lang::TypeScript, "src/c.tsx", &imp("./a", false)),
            vec![0]
        );
        assert_eq!(
            r.resolve(Lang::TypeScript, "src/c.tsx", &imp("./a.js", false)),
            vec![0]
        );
        assert_eq!(
            r.resolve(Lang::TypeScript, "src/c.tsx", &imp("./b", false)),
            vec![1]
        );
        assert_eq!(
            r.resolve(Lang::TypeScript, "src/b/index.ts", &imp("../c", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(Lang::JavaScript, "src/d.js", &imp("react", false)),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn kotlin_packages() {
        let files: Vec<(u32, &str)> = vec![(0, "a/A.kt"), (1, "a/B.kt"), (2, "b/C.kt")];
        let kt = vec![
            (0u32, "com.a", vec!["A"]),
            (1, "com.a", vec!["B"]),
            (2, "com.b", vec!["C"]),
        ];
        let r = Resolver::new(Path::new("/nonexistent"), &files, kt.into_iter());
        assert_eq!(
            r.resolve(Lang::Kotlin, "b/C.kt", &imp("com.a.A", false)),
            vec![0]
        );
        assert_eq!(
            r.resolve(Lang::Kotlin, "b/C.kt", &imp("com.a.A.Nested", false)),
            vec![0]
        );
        assert_eq!(
            r.resolve(Lang::Kotlin, "b/C.kt", &imp("com.a", true)),
            vec![0, 1]
        );
        assert_eq!(r.kotlin_package_peers("com.a", 0), vec![1]);
    }
}
