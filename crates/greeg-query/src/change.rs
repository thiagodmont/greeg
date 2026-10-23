//! What a checkout changed against a git base: the files, their changed
//! lines, and the definitions those lines touch on either side. Old sides are
//! read from git objects; nothing is checked out and the index is not
//! written.

use crate::Source;
use anyhow::{Context, Result, bail};
use greeg_index::rel::as_path;
use greeg_lang::{DefKind, Lang};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Which changes count: everything since `base` (staged, unstaged and
/// untracked files), or only what is staged.
#[derive(Clone, Debug)]
pub struct DiffSpec {
    /// A revision, or `REV...` for the merge base of REV and HEAD.
    pub base: String,
    pub staged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Not tracked and not ignored.
    Untracked,
}

impl FileStatus {
    pub fn name(self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefChange {
    Added,
    Removed,
    Renamed {
        from: String,
    },
    /// Its declaration changed (parameters, return type, supertypes).
    Signature {
        old: String,
        new: String,
    },
    /// Only its body changed.
    Body,
}

impl DefChange {
    pub fn name(&self) -> &'static str {
        match self {
            DefChange::Added => "added",
            DefChange::Removed => "removed",
            DefChange::Renamed { .. } => "renamed",
            DefChange::Signature { .. } => "signature",
            DefChange::Body => "body",
        }
    }
    /// Uses of the old name may break.
    pub fn breaking(&self) -> bool {
        matches!(
            self,
            DefChange::Removed | DefChange::Renamed { .. } | DefChange::Signature { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct ChangedDef {
    pub name: String,
    pub kind: DefKind,
    /// Enclosing definitions, outermost first (the definition itself excluded).
    pub parents: Vec<String>,
    /// On the new side; on the old side for a removed definition.
    pub line: u32,
    pub change: DefChange,
}

impl ChangedDef {
    /// The name its uses were written with before the change.
    pub fn old_name(&self) -> &str {
        match &self.change {
            DefChange::Renamed { from } => from,
            _ => &self.name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChangedFile {
    /// Root-relative path on the new side (the old one for a deleted file).
    pub rel: Vec<u8>,
    pub old_rel: Option<Vec<u8>>,
    pub status: FileStatus,
    /// Changed line ranges (first, last), 1-based, on each side.
    pub old_lines: Vec<(u32, u32)>,
    pub new_lines: Vec<(u32, u32)>,
    pub binary: bool,
    /// The file has a grammar, so its definitions were compared.
    pub parsed: bool,
    pub defs: Vec<ChangedDef>,
}

impl ChangedFile {
    /// Whether `line` (new side) is one of the changed lines.
    pub fn changed_line(&self, line: u32) -> bool {
        matches!(self.status, FileStatus::Added | FileStatus::Untracked)
            || self.new_lines.iter().any(|&(a, b)| a <= line && line <= b)
    }
}

#[derive(Clone, Debug)]
pub struct Changes {
    /// As asked (`HEAD`, `main...`).
    pub base: String,
    /// The commit compared against, abbreviated.
    pub commit: String,
    pub merge_base: bool,
    pub staged: bool,
    pub files: Vec<ChangedFile>,
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_command(root, args)
        .stdin(Stdio::null())
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// git in `root`, without taking optional locks (no index refresh is
/// written) and with paths unquoted.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.quotepath=off"])
        .args(args);
    c
}

fn trim_nl(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

/// The changes `spec` selects under `root`, with the definitions they touch.
pub fn changes(root: &Path, spec: &DiffSpec) -> Result<Changes> {
    if git(root, &["rev-parse", "--is-inside-work-tree"]).is_err() {
        bail!("{} is not inside a git work tree", root.display());
    }
    let (rev, merge_base) = match spec.base.strip_suffix("...") {
        Some(r) => (if r.is_empty() { "HEAD" } else { r }, true),
        None => (spec.base.as_str(), false),
    };
    let commit = if merge_base {
        trim_nl(&git(root, &["merge-base", rev, "HEAD"])?)
    } else {
        let want = format!("{rev}^{{commit}}");
        trim_nl(
            &git(root, &["rev-parse", "--verify", "--quiet", &want])
                .with_context(|| format!("unknown revision {rev}"))?,
        )
    };
    if commit.is_empty() {
        bail!("unknown revision {rev}");
    }
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--relative",
        "-M",
        "-U0",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    if spec.staged {
        args.push("--cached");
    }
    args.push(&commit);
    args.push("--");
    let mut files = parse_patch(&git(root, &args)?);
    if !spec.staged {
        let out = git(
            root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ],
        )?;
        for p in out.split(|&b| b == 0).filter(|p| !p.is_empty()) {
            files.push(ChangedFile {
                rel: p.to_vec(),
                old_rel: None,
                status: FileStatus::Untracked,
                old_lines: vec![],
                new_lines: vec![],
                binary: false,
                parsed: false,
                defs: vec![],
            });
        }
    }
    let mut olds: Vec<String> = Vec::new();
    let mut news: Vec<String> = Vec::new();
    let mut parse_ids = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if f.binary || !Lang::from_path(as_path(&f.rel)).has_grammar() {
            continue;
        }
        let old = f.old_rel.as_ref().unwrap_or(&f.rel);
        olds.push(match f.status {
            FileStatus::Added | FileStatus::Untracked => String::new(),
            _ => format!("{commit}:./{}", String::from_utf8_lossy(old)),
        });
        news.push(match (f.status, spec.staged) {
            (FileStatus::Deleted, _) | (_, false) => String::new(),
            (_, true) => format!(":./{}", String::from_utf8_lossy(&f.rel)),
        });
        parse_ids.push(i);
    }
    let old_blobs = blobs(root, &olds)?;
    let new_blobs = if spec.staged {
        blobs(root, &news)?
    } else {
        vec![None; news.len()]
    };
    for (k, &i) in parse_ids.iter().enumerate() {
        let f = &mut files[i];
        let new_src = match f.status {
            FileStatus::Deleted => None,
            _ if spec.staged => new_blobs[k].clone(),
            _ => read_worktree(root, &f.rel),
        };
        let old_src = old_blobs[k].clone();
        f.defs = changed_defs(f, old_src, new_src);
        f.parsed = true;
    }
    Ok(Changes {
        base: spec.base.clone(),
        commit: commit.chars().take(12).collect(),
        merge_base,
        staged: spec.staged,
        files,
    })
}

fn read_worktree(root: &Path, rel: &[u8]) -> Option<Vec<u8>> {
    let mut v = std::fs::read(root.join(as_path(rel))).ok()?;
    greeg_lang::transcode_utf16(&mut v);
    Some(v)
}

/// Objects by name (`REV:./path`, `:./path`) from one `git cat-file --batch`;
/// an empty name, a missing object or a name with a newline reads as `None`.
fn blobs(root: &Path, names: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
    let asked: Vec<usize> = (0..names.len())
        .filter(|&i| !names[i].is_empty() && !names[i].contains('\n'))
        .collect();
    let mut out = vec![None; names.len()];
    if asked.is_empty() {
        return Ok(out);
    }
    let mut child = git_command(root, &["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("run git cat-file")?;
    let mut stdin = child.stdin.take().context("git cat-file stdin")?;
    let input: String = asked.iter().map(|&i| format!("{}\n", names[i])).collect();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut r = BufReader::new(child.stdout.take().context("git cat-file stdout")?);
    for &i in &asked {
        let mut head = Vec::new();
        if r.read_until(b'\n', &mut head)? == 0 {
            break;
        }
        let head = String::from_utf8_lossy(&head);
        let mut parts = head.split_whitespace();
        let (_, kind, size) = (parts.next(), parts.next(), parts.next());
        let Some(size) = size.and_then(|s| s.parse::<usize>().ok()) else {
            continue; // `<name> missing`
        };
        let mut body = vec![0u8; size + 1];
        r.read_exact(&mut body)?;
        body.pop();
        if kind == Some("blob") {
            greeg_lang::transcode_utf16(&mut body);
            out[i] = Some(body);
        }
    }
    let _ = writer.join();
    let _ = child.wait();
    Ok(out)
}

/// A C-quoted path as git writes one with unusual bytes, or the path as is.
fn unquote(p: &[u8]) -> Vec<u8> {
    let Some(inner) = p.strip_prefix(b"\"").and_then(|p| p.strip_suffix(b"\"")) else {
        return p.to_vec();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        i += 1;
        if c != b'\\' || i >= inner.len() {
            out.push(c);
            continue;
        }
        let e = inner[i];
        i += 1;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&d @ b'0'..=b'7') = inner.get(i) {
                        v = v * 8 + (d - b'0') as u32;
                        i += 1;
                    }
                }
                out.push(v as u8);
            }
            other => out.push(other),
        }
    }
    out
}

/// `a/x` or `b/x` (quoted or not) to `x`; `/dev/null` to `None`.
fn side_path(p: &[u8]) -> Option<Vec<u8>> {
    let p = unquote(p);
    if p == b"/dev/null" {
        return None;
    }
    Some(
        p.strip_prefix(b"a/")
            .or_else(|| p.strip_prefix(b"b/"))
            .unwrap_or(&p)
            .to_vec(),
    )
}

/// `start[,count]` to the changed range, `None` for a count of 0.
fn range(s: &str) -> Option<(u32, u32)> {
    let (start, count) = match s.split_once(',') {
        Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
        None => (s.parse::<u32>().ok()?, 1),
    };
    (count > 0).then(|| (start, start + count - 1))
}

fn parse_patch(patch: &[u8]) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut cur: Option<ChangedFile> = None;
    let (mut old_path, mut new_path): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut finish =
        |cur: &mut Option<ChangedFile>, old: &mut Option<Vec<u8>>, new: &mut Option<Vec<u8>>| {
            if let Some(mut f) = cur.take() {
                let (o, n) = (old.take(), new.take());
                f.status = match (&o, &n) {
                    (None, Some(_)) => FileStatus::Added,
                    (Some(_), None) => FileStatus::Deleted,
                    (Some(a), Some(b)) if a != b => FileStatus::Renamed,
                    _ => FileStatus::Modified,
                };
                f.rel = n.clone().or(o.clone()).unwrap_or_default();
                f.old_rel = if f.status == FileStatus::Renamed {
                    o
                } else {
                    None
                };
                if !f.rel.is_empty() {
                    files.push(f);
                }
            }
        };
    for line in patch.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            finish(&mut cur, &mut old_path, &mut new_path);
            cur = Some(ChangedFile {
                rel: Vec::new(),
                old_rel: None,
                status: FileStatus::Modified,
                old_lines: vec![],
                new_lines: vec![],
                binary: false,
                parsed: false,
                defs: vec![],
            });
            // both sides from the header, for a change without ---/+++ lines
            // (a pure rename, a mode change); paths with spaces are unquoted
            // only when they carry no " b/" of their own
            let s = String::from_utf8_lossy(rest).into_owned();
            if let Some((a, b)) = s.split_once(" b/").filter(|_| !rest.starts_with(b"\"")) {
                old_path = side_path(a.as_bytes());
                new_path = Some(b.as_bytes().to_vec());
            }
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };
        if let Some(p) = line.strip_prefix(b"--- ") {
            old_path = side_path(p);
        } else if let Some(p) = line.strip_prefix(b"+++ ") {
            new_path = side_path(p);
        } else if let Some(p) = line.strip_prefix(b"rename from ") {
            old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix(b"rename to ") {
            new_path = Some(unquote(p));
        } else if line.starts_with(b"new file mode") {
            old_path = None;
        } else if line.starts_with(b"deleted file mode") {
            new_path = None;
        } else if line.starts_with(b"Binary files ") {
            f.binary = true;
        } else if let Some(h) = line.strip_prefix(b"@@ -") {
            let h = String::from_utf8_lossy(h);
            let mut it = h.split_whitespace();
            if let (Some(o), Some(n)) = (it.next(), it.next()) {
                f.old_lines.extend(range(o));
                f.new_lines.extend(n.strip_prefix('+').and_then(range));
            }
        }
    }
    finish(&mut cur, &mut old_path, &mut new_path);
    files
}

/// One side's definitions with their line spans.
struct Side {
    src: Source,
    syms: Vec<greeg_lang::sym::Symbol>,
    ex: greeg_lang::sym::Extract,
    spans: Vec<(u32, u32)>,
}

impl Side {
    fn parse(rel: &[u8], bytes: Vec<u8>) -> Side {
        let path = String::from_utf8_lossy(rel).into_owned();
        let lang = Lang::from_path(Path::new(&path));
        let mut ex = greeg_lang::sym::extract(lang, greeg_lang::sym::is_tsx(&path), &bytes);
        let src = Source::new(bytes);
        let syms = std::mem::take(&mut ex.symbols);
        let spans = syms
            .iter()
            .map(|s| (s.line, src.end_line(s.start, s.end)))
            .collect();
        Side {
            src,
            syms,
            ex,
            spans,
        }
    }
    fn name(&self, i: usize) -> &str {
        self.ex.name(&self.syms[i], &self.src.bytes)
    }
    /// Names of the enclosing definitions, outermost first.
    fn parents(&self, i: usize) -> Vec<String> {
        let mut v = Vec::new();
        let mut cur = self.syms[i].parent;
        while let Some(p) = cur {
            v.push(self.name(p as usize).to_string());
            cur = self.syms[p as usize].parent;
        }
        v.reverse();
        v
    }
    fn key(&self, i: usize) -> (DefKind, Vec<String>, String) {
        (self.syms[i].kind, self.parents(i), self.name(i).to_string())
    }
    /// The innermost definition holding `line`.
    fn innermost(&self, line: u32) -> Option<usize> {
        (0..self.syms.len())
            .filter(|&i| self.spans[i].0 <= line && line <= self.spans[i].1)
            .min_by_key(|&i| self.spans[i].1 - self.spans[i].0)
    }
    /// The definitions changed lines fall in, innermost each.
    fn touched(&self, ranges: &[(u32, u32)]) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for &(a, b) in ranges {
            for line in a..=b.min(a.saturating_add(20_000)) {
                if let Some(i) = self.innermost(line)
                    && !out.contains(&i)
                {
                    out.push(i);
                }
            }
        }
        out
    }
    /// The declaration up to its body, whitespace collapsed: what callers
    /// depend on. It starts on the name's line, after any decorators.
    fn signature(&self, i: usize) -> String {
        let s = &self.syms[i];
        let bytes = &self.src.bytes;
        let name_line = self.src.line_of(s.name_start);
        let line_start = bytes[..(s.name_start as usize).min(bytes.len())]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |k| k + 1) as u32;
        let from = if self.src.line_of(s.start) < name_line {
            line_start
        } else {
            s.start
        };
        let b = &bytes[from as usize..(s.end as usize).min(bytes.len()).max(from as usize)];
        let mut depth = 0i32;
        let mut end = b.len();
        for (k, &c) in b.iter().enumerate() {
            match c {
                b'(' | b'[' | b'<' => depth += 1,
                b')' | b']' => depth -= 1,
                b'>' if k > 0 && b[k - 1] != b'-' && b[k - 1] != b'=' => depth -= 1,
                b'{' if depth <= 0 => {
                    end = k;
                    break;
                }
                b'\n' if depth <= 0 => {
                    end = k;
                    break;
                }
                b'=' if depth <= 0
                    && b.get(k + 1) != Some(&b'>')
                    && b.get(k + 1) != Some(&b'=') =>
                {
                    end = k;
                    break;
                }
                _ => {}
            }
        }
        String::from_utf8_lossy(&b[..end])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches(':')
            .trim_end()
            .to_string()
    }
}

fn changed_defs(f: &ChangedFile, old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Vec<ChangedDef> {
    let old_rel = f.old_rel.as_ref().unwrap_or(&f.rel);
    let old = old.map(|b| Side::parse(old_rel, b));
    let new = new.map(|b| Side::parse(&f.rel, b));
    let def = |side: &Side, i: usize, change: DefChange| ChangedDef {
        name: side.name(i).to_string(),
        kind: side.syms[i].kind,
        parents: side.parents(i),
        line: side.syms[i].line,
        change,
    };
    let whole = |side: &Side, change: DefChange| -> Vec<ChangedDef> {
        (0..side.syms.len())
            .filter(|&i| side.syms[i].kind != DefKind::Impl)
            .map(|i| def(side, i, change.clone()))
            .collect()
    };
    match (&old, &new) {
        (None, None) => return vec![],
        (None, Some(n)) => return whole(n, DefChange::Added),
        (Some(o), None) => return whole(o, DefChange::Removed),
        _ => {}
    }
    let (o, n) = (old.as_ref().unwrap(), new.as_ref().unwrap());
    let find = |side: &Side, key: &(DefKind, Vec<String>, String)| {
        (0..side.syms.len()).find(|&i| side.key(i) == *key)
    };
    let mut out: Vec<ChangedDef> = Vec::new();
    let mut removed: Vec<usize> = Vec::new();
    let mut added: Vec<usize> = Vec::new();
    let mut seen_new: Vec<usize> = Vec::new();
    for i in o.touched(&f.old_lines) {
        match find(n, &o.key(i)) {
            Some(j) => {
                if !seen_new.contains(&j) {
                    seen_new.push(j);
                    out.push(modified(o, i, n, j, &def));
                }
            }
            None => removed.push(i),
        }
    }
    for j in n.touched(&f.new_lines) {
        if seen_new.contains(&j) {
            continue;
        }
        match find(o, &n.key(j)) {
            Some(i) => {
                seen_new.push(j);
                out.push(modified(o, i, n, j, &def));
            }
            None => added.push(j),
        }
    }
    // the only removed and the only added definition of one kind in one
    // scope: a rename; with more of either, which became which is a guess
    let scope = |s: &Side, i: usize| (s.syms[i].kind, s.parents(i));
    let mut renamed: Vec<(usize, usize)> = Vec::new();
    for &j in &added {
        let olds: Vec<usize> = removed
            .iter()
            .copied()
            .filter(|&i| scope(o, i) == scope(n, j))
            .collect();
        let news = added
            .iter()
            .filter(|&&k| scope(n, k) == scope(n, j))
            .count();
        if let [i] = olds[..]
            && news == 1
        {
            renamed.push((i, j));
        }
    }
    for j in added {
        match renamed.iter().find(|&&(_, k)| k == j) {
            Some(&(i, _)) => out.push(def(
                n,
                j,
                DefChange::Renamed {
                    from: o.name(i).to_string(),
                },
            )),
            None => out.push(def(n, j, DefChange::Added)),
        }
    }
    out.extend(
        removed
            .into_iter()
            .filter(|i| !renamed.iter().any(|(r, _)| r == i))
            .map(|i| def(o, i, DefChange::Removed)),
    );
    out.retain(|d| d.kind != DefKind::Impl);
    out.sort_by_key(|d| (d.change == DefChange::Removed, d.line));
    out
}

fn modified(
    o: &Side,
    i: usize,
    n: &Side,
    j: usize,
    def: &dyn Fn(&Side, usize, DefChange) -> ChangedDef,
) -> ChangedDef {
    let (a, b) = (o.signature(i), n.signature(j));
    def(
        n,
        j,
        if a == b {
            DefChange::Body
        } else {
            DefChange::Signature { old: a, new: b }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_hunks_and_statuses() {
        let patch = b"diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3 +3,2 @@ fn x\n-a\n+b\n+c\n@@ -10,2 +11,0 @@\n-x\n-y\ndiff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\ndiff --git a/old name.kt b/new name.kt\nsimilarity index 100%\nrename from old name.kt\nrename to new name.kt\ndiff --git a/new.ts b/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/new.ts\n@@ -0,0 +1 @@\n+x\ndiff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\ndiff --git \"a/t\\303\\251\\tx.py\" \"b/t\\303\\251\\tx.py\"\n--- \"a/t\\303\\251\\tx.py\"\n+++ \"b/t\\303\\251\\tx.py\"\n@@ -1 +1 @@\n-a\n+b\n";
        let f = parse_patch(patch);
        type Row = (String, FileStatus, Vec<(u32, u32)>, Vec<(u32, u32)>, bool);
        let got: Vec<Row> = f
            .iter()
            .map(|f| {
                (
                    String::from_utf8_lossy(&f.rel).into_owned(),
                    f.status,
                    f.old_lines.clone(),
                    f.new_lines.clone(),
                    f.binary,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "src/a.rs".into(),
                    FileStatus::Modified,
                    vec![(3, 3), (10, 11)],
                    vec![(3, 4)],
                    false
                ),
                (
                    "gone.py".into(),
                    FileStatus::Deleted,
                    vec![(1, 2)],
                    vec![],
                    false
                ),
                (
                    "new name.kt".into(),
                    FileStatus::Renamed,
                    vec![],
                    vec![],
                    false
                ),
                (
                    "new.ts".into(),
                    FileStatus::Added,
                    vec![],
                    vec![(1, 1)],
                    false
                ),
                ("img.png".into(), FileStatus::Modified, vec![], vec![], true),
                (
                    "té\tx.py".into(),
                    FileStatus::Modified,
                    vec![(1, 1)],
                    vec![(1, 1)],
                    false
                ),
            ]
        );
        assert_eq!(f[2].old_rel.as_deref(), Some(&b"old name.kt"[..]));
    }

    fn file(old: &[(u32, u32)], new: &[(u32, u32)]) -> ChangedFile {
        ChangedFile {
            rel: b"m.py".to_vec(),
            old_rel: None,
            status: FileStatus::Modified,
            old_lines: old.to_vec(),
            new_lines: new.to_vec(),
            binary: false,
            parsed: false,
            defs: vec![],
        }
    }

    fn summary(d: &[ChangedDef]) -> Vec<(String, &'static str, u32)> {
        d.iter()
            .map(|d| {
                let name = match &d.change {
                    DefChange::Renamed { from } => format!("{from}->{}", d.name),
                    _ => d.name.clone(),
                };
                (name, d.change.name(), d.line)
            })
            .collect()
    }

    #[test]
    fn definitions_are_compared_across_sides() {
        let old = b"def keep(a):\n    return a\n\ndef sig(a):\n    return a\n\ndef gone():\n    pass\n\ndef old_name():\n    pass\n\nclass C:\n    def m(self):\n        return 1\n".to_vec();
        let new = b"def keep(a):\n    return a + 1\n\ndef sig(a, b):\n    return a\n\ndef new_name():\n    pass\n\nclass C:\n    def m(self):\n        return 2\n".to_vec();
        // keep: body; sig: signature; gone and old_name removed, new_name
        // added (two removed: which one became new_name is a guess); C.m body
        let f = file(
            &[(2, 2), (4, 4), (7, 11), (15, 15)],
            &[(2, 2), (4, 4), (7, 8), (12, 12)],
        );
        let d = changed_defs(&f, Some(old.clone()), Some(new));
        assert_eq!(
            summary(&d),
            vec![
                ("keep".into(), "body", 1),
                ("sig".into(), "signature", 4),
                ("new_name".into(), "added", 7),
                ("m".into(), "body", 11),
                ("gone".into(), "removed", 7),
                ("old_name".into(), "removed", 10),
            ]
        );
        // one removed and one added in a scope: a rename
        let new = b"def keep(a):\n    return a\n\ndef sig(a):\n    return a\n\ndef gone():\n    pass\n\ndef fresh_name():\n    pass\n\nclass C:\n    def m(self):\n        return 1\n".to_vec();
        let f = file(&[(10, 10)], &[(10, 10)]);
        let renamed = changed_defs(&f, Some(old), Some(new));
        assert_eq!(
            summary(&renamed),
            vec![("old_name->fresh_name".into(), "renamed", 10)]
        );
        let sig = d.iter().find(|d| d.name == "sig").unwrap();
        assert_eq!(
            sig.change,
            DefChange::Signature {
                old: "def sig(a)".into(),
                new: "def sig(a, b)".into()
            }
        );
        assert_eq!(d.iter().find(|d| d.name == "m").unwrap().parents, vec!["C"]);
    }

    #[test]
    fn whole_files_add_or_remove_every_definition() {
        let src = b"fn a() {}\nstruct S;\nimpl S { fn b(&self) {} }\n".to_vec();
        let mut f = file(&[], &[]);
        f.rel = b"x.rs".to_vec();
        let added = changed_defs(&f, None, Some(src.clone()));
        assert!(added.iter().all(|d| d.change == DefChange::Added));
        assert_eq!(
            added.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "S", "b"]
        );
        let removed = changed_defs(&f, Some(src), None);
        assert!(removed.iter().all(|d| d.change == DefChange::Removed));
    }

    #[test]
    fn signatures_stop_at_the_body() {
        let cases: [(&str, &[u8], &str); 4] = [
            (
                "a.rs",
                b"pub fn f(\n    a: u32,\n    b: Vec<u8>,\n) -> Option<u32> {\n    None\n}\n",
                "pub fn f( a: u32, b: Vec<u8>, ) -> Option<u32>",
            ),
            (
                "a.py",
                b"def f(a,\n      b=1):\n    return a\n",
                "def f(a, b=1)",
            ),
            (
                "a.kt",
                b"fun String.f(start: Int = 0): String = decode(start)\n",
                "fun String.f(start: Int = 0): String",
            ),
            (
                "a.ts",
                b"function f<T>(x: T): T {\n  return x;\n}\n",
                "function f<T>(x: T): T",
            ),
        ];
        for (rel, src, want) in cases {
            let s = Side::parse(rel.as_bytes(), src.to_vec());
            assert_eq!(s.signature(0), want, "{rel}");
        }
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        assert_eq!(unquote(b"\"a\\\\b\\\"c\""), b"a\\b\"c");
        assert_eq!(unquote(b"plain"), b"plain");
        assert_eq!(side_path(b"/dev/null"), None);
    }
}
