//! Import resolution at index time (ARCHITECTURE.md): map an import statement
//! of a file to the file ids it refers to. Path-based for Python, Rust and
//! JS/TS (the nearest `tsconfig.json`/`jsconfig.json` supplies `paths` and
//! `baseUrl`, and workspace packages resolve by name); package-table based
//! for Kotlin. Unresolved imports keep their raw text in `spans.bin` and
//! produce no edge.

use greeg_lang::Lang;
use greeg_lang::sym::Import;
use hashbrown::HashMap;
use std::path::Path;

#[derive(Default, Clone, Debug)]
pub struct FileCtx {
    pub rust_src: String,
    pub rust_mod_dir: String,
    /// Index into `Resolver::ts_configs` of the nearest tsconfig, if any.
    pub ts: Option<usize>,
}

/// One `tsconfig.json` (with its `extends` chain folded in).
#[derive(Debug, Default, Clone)]
pub struct TsConfig {
    /// Repo-relative directory of the config file ("" = root).
    pub dir: String,
    /// Effective `baseUrl`, repo-relative.
    pub base_url: Option<String>,
    /// `paths`: (pattern, targets), targets repo-relative and possibly containing `*`.
    pub paths: Vec<(String, Vec<String>)>,
}

/// A workspace package (from `package.json` `name`): its directory and `main`.
#[derive(Debug, Default, Clone)]
pub struct JsPackage {
    pub dir: String,
    pub main: Option<String>,
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
    /// tsconfig/jsconfig files, loaded once; `ts_by_dir` maps their directories.
    ts_configs: Vec<TsConfig>,
    ts_by_dir: HashMap<String, usize>,
    /// Workspace packages by name (`package.json` files named by the root's `workspaces`).
    js_packages: HashMap<String, JsPackage>,
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
        let mut ts_dirs: Vec<String> = Vec::new();
        let mut pkg_jsons: Vec<String> = Vec::new();
        let mut cargo_tomls: Vec<&str> = Vec::new();
        for &(id, rel) in files {
            paths.insert(rel, id);
            let name = rel.rsplit('/').next().unwrap_or(rel);
            match name {
                "tsconfig.json" | "jsconfig.json" => {
                    let d = dir_of(rel).to_string();
                    // tsconfig wins over a sibling jsconfig
                    if name == "tsconfig.json" || !ts_dirs.contains(&d) {
                        ts_dirs.push(d);
                    }
                }
                "package.json" => pkg_jsons.push(rel.to_string()),
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
                "Cargo.toml" => cargo_tomls.push(rel),
                _ => {}
            }
        }
        // package names: one small read per Cargo.toml, on a few threads (a
        // delta on rust-lang/rust reads 384 of them; serial that is 5 ms)
        for (rel, name) in
            cargo_tomls
                .iter()
                .zip(read_many(root, &cargo_tomls, 4, cargo_package_name))
        {
            let d = dir_of(rel).to_string();
            let pkg = name
                .unwrap_or_else(|| d.rsplit('/').next().unwrap_or("").to_string())
                .replace('-', "_");
            let src = join(&d, "src");
            rust_crates.push((d, pkg, src));
        }
        py_roots.sort_by_key(|b| std::cmp::Reverse(b.len()));
        rust_crates.sort_by_key(|c| std::cmp::Reverse(c.0.len()));
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
        // tsconfig files: the nearest one (by directory) governs a JS/TS file
        ts_dirs.sort();
        ts_dirs.dedup();
        let mut ts_configs: Vec<TsConfig> = Vec::with_capacity(ts_dirs.len());
        for d in &ts_dirs {
            let file = if paths.contains_key(join(d, "tsconfig.json").as_str()) {
                "tsconfig.json"
            } else {
                "jsconfig.json"
            };
            ts_configs.push(load_tsconfig(root, &join(d, file), 0));
        }
        let ts_by_dir = ts_configs
            .iter()
            .enumerate()
            .map(|(i, c)| (c.dir.clone(), i))
            .collect();
        let js_packages = workspace_packages(root, &pkg_jsons);
        Resolver {
            paths,
            py_roots,
            rust_crates,
            rust_by_name,
            rust_by_dir,
            kt_packages,
            kt_syms,
            ts_configs,
            ts_by_dir,
            js_packages,
        }
    }

    pub fn ts_configs(&self) -> &[TsConfig] {
        &self.ts_configs
    }
    pub fn js_packages(&self) -> &HashMap<String, JsPackage> {
        &self.js_packages
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
        if matches!(lang, Lang::JavaScript | Lang::TypeScript) && !self.ts_by_dir.is_empty() {
            let mut d = dir_of(from_rel);
            loop {
                if let Some(&i) = self.ts_by_dir.get(d) {
                    ctx.ts = Some(i);
                    break;
                }
                if d.is_empty() {
                    break;
                }
                d = parent(d);
            }
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
                self.js(from_rel, ctx, &im.module).into_iter().collect()
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

    fn js(&self, from_rel: &str, ctx: &FileCtx, module: &str) -> Option<u32> {
        if module.starts_with("./") || module.starts_with("../") || module == "." || module == ".."
        {
            return self.js_probe(&normalize(&join(dir_of(from_rel), module)));
        }
        if module.starts_with('/') || module.starts_with("node:") {
            return None;
        }
        // tsconfig `paths` (longest matching prefix first), then `baseUrl`
        if let Some(cfg) = ctx.ts.and_then(|i| self.ts_configs.get(i)) {
            let mut best: Option<(usize, &str, &[String])> = None;
            for (pat, targets) in &cfg.paths {
                let m = match pat.split_once('*') {
                    None => (pat == module).then_some(""),
                    Some((pre, suf)) => (module.len() >= pre.len() + suf.len()
                        && module.starts_with(pre)
                        && module.ends_with(suf))
                    .then(|| &module[pre.len()..module.len() - suf.len()]),
                };
                if let Some(star) = m
                    && best.map(|(l, _, _)| pat.len() > l).unwrap_or(true)
                {
                    best = Some((pat.len(), star, targets));
                }
            }
            if let Some((_, star, targets)) = best {
                for t in targets {
                    let p = normalize(&t.replacen('*', star, 1));
                    if let Some(id) = self.js_probe(&p) {
                        return Some(id);
                    }
                }
            }
            if let Some(b) = &cfg.base_url
                && let Some(id) = self.js_probe(&normalize(&join(b, module)))
            {
                return Some(id);
            }
        }
        // workspace package by name: `@scope/name[/sub]` or `name[/sub]`
        if !self.js_packages.is_empty() {
            let cut = if module.starts_with('@') {
                module.match_indices('/').nth(1).map(|(i, _)| i)
            } else {
                module.find('/')
            };
            let (name, sub) = match cut {
                Some(i) => (&module[..i], &module[i + 1..]),
                None => (module, ""),
            };
            if let Some(pkg) = self.js_packages.get(name) {
                if sub.is_empty() {
                    if let Some(m) = &pkg.main
                        && let Some(id) = self.js_probe(&normalize(&join(&pkg.dir, m)))
                    {
                        return Some(id);
                    }
                    for c in ["src/index", "index", "src/main", "lib/index"] {
                        if let Some(id) = self.js_probe(&join(&pkg.dir, c)) {
                            return Some(id);
                        }
                    }
                } else {
                    for base in [pkg.dir.clone(), join(&pkg.dir, "src")] {
                        if let Some(id) = self.js_probe(&normalize(&join(&base, sub))) {
                            return Some(id);
                        }
                    }
                }
            }
        }
        None
    }

    /// A repo-relative path as written in an import: the file itself, the
    /// stem with a source extension (`.js` may name a `.ts`), or a directory index.
    fn js_probe(&self, p: &str) -> Option<u32> {
        if let Some(id) = self.get(p) {
            return Some(id);
        }
        let stem = if let Some(s) = p
            .strip_suffix(".js")
            .or_else(|| p.strip_suffix(".jsx"))
            .or_else(|| p.strip_suffix(".mjs"))
            .or_else(|| p.strip_suffix(".cjs"))
        {
            s
        } else {
            p
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
            if let Some(id) = self.get(&join(p, idx)) {
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
/// Strip `//` and `/* */` comments and trailing commas so `tsconfig.json`
/// (JSON with comments) parses as JSON.
pub fn strip_jsonc(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&src[start..i]);
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if c == b',' {
            // trailing comma: next non-space is `]` or `}`
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b']' || b[j] == b'}') {
                i += 1;
                continue;
            }
            out.push(',');
            i += 1;
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

/// Parse a tsconfig with its `extends` chain (`rel` repo-relative). `paths`
/// targets are made repo-relative against the effective `baseUrl`, else the
/// directory of the config that declared them.
fn load_tsconfig(root: &Path, rel: &str, depth: usize) -> TsConfig {
    type Patterns = Vec<(String, Vec<String>)>;
    struct Raw {
        base_url: Option<String>,
        /// (declaring config dir, patterns)
        paths: Option<(String, Patterns)>,
    }
    fn raw(root: &Path, rel: &str, depth: usize) -> Raw {
        let mut r = Raw {
            base_url: None,
            paths: None,
        };
        if depth > 8 {
            return r;
        }
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            return r;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(&text)) else {
            return r;
        };
        let dir = dir_of(rel).to_string();
        // parents first; the child's own settings override
        let parents: Vec<String> = match v.get("extends") {
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        for p in parents {
            if let Some(prel) = resolve_extends(root, &dir, &p) {
                let pr = raw(root, &prel, depth + 1);
                if r.base_url.is_none() {
                    r.base_url = pr.base_url;
                }
                if r.paths.is_none() {
                    r.paths = pr.paths;
                }
            }
        }
        if let Some(co) = v.get("compilerOptions") {
            if let Some(b) = co.get("baseUrl").and_then(|x| x.as_str()) {
                r.base_url = Some(normalize(&join(&dir, b)));
            }
            if let Some(serde_json::Value::Object(m)) = co.get("paths") {
                let mut pats = Vec::with_capacity(m.len());
                for (k, v) in m {
                    let targets: Vec<String> = match v {
                        serde_json::Value::Array(a) => a
                            .iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect(),
                        serde_json::Value::String(s) => vec![s.clone()],
                        _ => Vec::new(),
                    };
                    pats.push((k.clone(), targets));
                }
                r.paths = Some((dir.clone(), pats));
            }
        }
        r
    }
    let r = raw(root, rel, depth);
    let dir = dir_of(rel).to_string();
    let paths = match r.paths {
        Some((decl_dir, pats)) => {
            let base = r.base_url.clone().unwrap_or(decl_dir);
            pats.into_iter()
                .map(|(k, ts)| (k, ts.iter().map(|t| normalize(&join(&base, t))).collect()))
                .collect()
        }
        None => Vec::new(),
    };
    TsConfig {
        dir,
        base_url: r.base_url,
        paths,
    }
}

/// Repo-relative path of an `extends` target: a relative file, or a package
/// under the nearest `node_modules` (which the walker does not index, so it
/// is probed on disk).
fn resolve_extends(root: &Path, dir: &str, target: &str) -> Option<String> {
    let candidates = |base: &str| -> Vec<String> {
        let p = normalize(&join(base, target));
        let mut v = vec![p.clone()];
        if !p.ends_with(".json") {
            v.push(format!("{p}.json"));
            v.push(join(&p, "tsconfig.json"));
        }
        v
    };
    if target.starts_with('.') || target.starts_with('/') {
        return candidates(dir).into_iter().find(|c| root.join(c).is_file());
    }
    let mut d = dir;
    loop {
        let nm = join(d, "node_modules");
        if let Some(c) = candidates(&nm).into_iter().find(|c| root.join(c).is_file()) {
            return Some(c);
        }
        if d.is_empty() {
            return None;
        }
        d = parent(d);
    }
}

/// Workspace packages: the `package.json` files matched by the root
/// `package.json` `workspaces` globs (or `pnpm-workspace.yaml` packages),
/// keyed by their `name`. Only those files are read.
fn workspace_packages(root: &Path, pkg_jsons: &[String]) -> HashMap<String, JsPackage> {
    let mut out = HashMap::new();
    if !pkg_jsons.iter().any(|p| p == "package.json") {
        return out;
    }
    let mut globs: Vec<String> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let ws = match v.get("workspaces") {
            Some(serde_json::Value::Array(a)) => Some(a.clone()),
            Some(serde_json::Value::Object(o)) => {
                o.get("packages").and_then(|p| p.as_array()).cloned()
            }
            _ => None,
        };
        for g in ws.unwrap_or_default() {
            if let Some(s) = g.as_str() {
                globs.push(s.to_string());
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        let mut in_pkgs = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with("packages:") {
                in_pkgs = true;
                continue;
            }
            if in_pkgs {
                if let Some(rest) = t.strip_prefix("- ") {
                    globs.push(
                        rest.trim()
                            .trim_matches(|c| c == '"' || c == '\'')
                            .to_string(),
                    );
                } else if !t.is_empty() && !t.starts_with('#') {
                    in_pkgs = false;
                }
            }
        }
    }
    if globs.is_empty() {
        return out;
    }
    let globs: Vec<String> = globs
        .iter()
        .map(|g| g.trim_start_matches("./").trim_end_matches('/').to_string())
        .filter(|g| !g.starts_with('!'))
        .collect();
    for rel in pkg_jsons {
        let d = dir_of(rel);
        if d.is_empty() || !globs.iter().any(|g| glob_dir(g, d)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let main = ["source", "module", "main", "types"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
            .map(|m| m.trim_start_matches("./").to_string());
        out.entry(name.to_string()).or_insert(JsPackage {
            dir: d.to_string(),
            main,
        });
    }
    out
}

/// Workspace glob match on a directory: `a/*` one level, `a/**` any depth,
/// `a/b` exact; `*` inside a segment matches within that segment.
fn glob_dir(glob: &str, dir: &str) -> bool {
    fn seg_match(g: &str, s: &str) -> bool {
        match g.split_once('*') {
            None => g == s,
            Some((pre, suf)) => {
                s.len() >= pre.len() + suf.len() && s.starts_with(pre) && s.ends_with(suf)
            }
        }
    }
    let gs: Vec<&str> = glob.split('/').filter(|s| !s.is_empty()).collect();
    let ds: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    fn go(gs: &[&str], ds: &[&str]) -> bool {
        match (gs.first(), ds.first()) {
            (None, None) => true,
            (Some(&"**"), _) => (0..=ds.len()).any(|k| go(&gs[1..], &ds[k..])),
            (Some(g), Some(d)) => seg_match(g, d) && go(&gs[1..], &ds[1..]),
            _ => false,
        }
    }
    go(&gs, &ds)
}

/// Apply `f` to `root/rel` for every `rels` entry on up to `threads` threads,
/// preserving order.
fn read_many<T: Send>(
    root: &Path,
    rels: &[&str],
    threads: usize,
    f: impl Fn(&Path) -> T + Sync,
) -> Vec<T> {
    let n = rels.len();
    let threads = threads.clamp(1, 8).min(n.max(1));
    if threads <= 1 || n < 16 {
        return rels.iter().map(|r| f(&root.join(r))).collect();
    }
    let chunk = n.div_ceil(threads);
    let mut parts: Vec<Vec<T>> = Vec::with_capacity(threads);
    std::thread::scope(|sc| {
        let handles: Vec<_> = rels
            .chunks(chunk)
            .map(|c| {
                let f = &f;
                sc.spawn(move || c.iter().map(|r| f(&root.join(r))).collect::<Vec<T>>())
            })
            .collect();
        for h in handles {
            parts.push(h.join().unwrap());
        }
    });
    parts.into_iter().flatten().collect()
}

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

    fn temp_root(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("greeg-resolve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn jsonc() {
        let src = "{\n  // c\n  \"a\": [1, 2,], /* x */ \"b\": \"s//t\",\n}\n";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"].as_array().unwrap().len(), 2);
        assert_eq!(v["b"], "s//t");
    }

    #[test]
    fn workspace_globs() {
        assert!(glob_dir("packages/*", "packages/a"));
        assert!(!glob_dir("packages/*", "packages/a/b"));
        assert!(glob_dir("apps/**", "apps/x/y"));
        assert!(glob_dir("apps/**", "apps"));
        assert!(glob_dir("tools/cli", "tools/cli"));
        assert!(glob_dir("packages/@scope-*", "packages/@scope-ui"));
        assert!(!glob_dir("packages/*", "tools/a"));
    }

    #[test]
    fn tsconfig_paths_and_base_url() {
        let root = temp_root("ts");
        write(
            &root,
            "tsconfig.base.json",
            "{ \"compilerOptions\": { \"baseUrl\": \"./src\", \"paths\": { \"@lib/*\": [\"lib/old/*\"], \"utils\": [\"lib/utils/index.ts\"] } } }",
        );
        // next-style alias, comments and trailing commas, extends the base
        write(
            &root,
            "tsconfig.json",
            "{\n  // app config\n  \"extends\": \"./tsconfig.base\",\n  \"compilerOptions\": {\n    \"paths\": { \"@/*\": [\"./*\"], \"@lib/*\": [\"lib/*\"], },\n  },\n}\n",
        );
        // a nested package with its own jsconfig
        write(
            &root,
            "packages/web/jsconfig.json",
            "{ \"compilerOptions\": { \"baseUrl\": \".\" } }",
        );
        let files: Vec<(u32, &str)> = vec![
            (0, "tsconfig.base.json"),
            (1, "tsconfig.json"),
            (2, "src/lib/a.ts"),
            (3, "src/lib/utils/index.ts"),
            (4, "src/components/button.tsx"),
            (5, "src/app/page.tsx"),
            (6, "packages/web/jsconfig.json"),
            (7, "packages/web/components/nav.js"),
            (8, "packages/web/pages/index.js"),
            (9, "src/lib/b/index.ts"),
        ];
        let r = Resolver::new(&root, &files, std::iter::empty());
        let cfg = r.ts_configs();
        assert_eq!(cfg.len(), 2);
        let top = &cfg[cfg.iter().position(|c| c.dir.is_empty()).unwrap()];
        assert_eq!(
            top.base_url.as_deref(),
            Some("src"),
            "baseUrl inherited from the base config"
        );
        let ts = Lang::TypeScript;
        // `@/*` → `./*` relative to baseUrl (src)
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("@/components/button", false)),
            vec![4]
        );
        // the child's `@lib/*` overrides the parent's; targets are relative to the inherited baseUrl
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("@lib/a", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("@lib/b", false)),
            vec![9]
        );
        // exact pattern without a star comes from the parent (paths replaced wholesale by the child)
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("utils", false)),
            Vec::<u32>::new()
        );
        // bare baseUrl-relative import
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("lib/utils", false)),
            vec![3]
        );
        // relative imports still work and packages stay external
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("../lib/a", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(ts, "src/app/page.tsx", &imp("react", false)),
            Vec::<u32>::new()
        );
        // nested jsconfig governs its subtree
        assert_eq!(
            r.resolve(
                Lang::JavaScript,
                "packages/web/pages/index.js",
                &imp("components/nav", false)
            ),
            vec![7]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_packages_by_name() {
        let root = temp_root("ws");
        write(
            &root,
            "package.json",
            "{ \"name\": \"mono\", \"workspaces\": [\"packages/*\", \"apps/**\"] }",
        );
        write(
            &root,
            "packages/ui/package.json",
            "{ \"name\": \"@acme/ui\", \"main\": \"dist/index.js\", \"source\": \"src/index.ts\" }",
        );
        write(
            &root,
            "packages/core/package.json",
            "{ \"name\": \"core\" }",
        );
        write(&root, "apps/web/package.json", "{ \"name\": \"web\" }");
        // not a workspace member: must not be read
        write(
            &root,
            "fixtures/x/package.json",
            "{ \"name\": \"fixture\" }",
        );
        let files: Vec<(u32, &str)> = vec![
            (0, "package.json"),
            (1, "packages/ui/package.json"),
            (2, "packages/ui/src/index.ts"),
            (3, "packages/ui/src/button.tsx"),
            (4, "packages/core/package.json"),
            (5, "packages/core/index.ts"),
            (6, "apps/web/package.json"),
            (7, "apps/web/src/main.ts"),
            (8, "fixtures/x/package.json"),
            (9, "fixtures/x/index.ts"),
        ];
        let r = Resolver::new(&root, &files, std::iter::empty());
        assert_eq!(r.js_packages().len(), 3);
        let ts = Lang::TypeScript;
        assert_eq!(
            r.resolve(ts, "apps/web/src/main.ts", &imp("@acme/ui", false)),
            vec![2]
        );
        assert_eq!(
            r.resolve(ts, "apps/web/src/main.ts", &imp("@acme/ui/button", false)),
            vec![3]
        );
        assert_eq!(
            r.resolve(ts, "apps/web/src/main.ts", &imp("core", false)),
            vec![5]
        );
        assert_eq!(
            r.resolve(ts, "apps/web/src/main.ts", &imp("fixture", false)),
            Vec::<u32>::new()
        );
        let _ = std::fs::remove_dir_all(&root);
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
