//! Stamps the build so `greeg --version` and the stats records tell a
//! development build from the release it was cut from: `0.4.0` at the
//! release tag, `0.4.0+ff9011a` from another commit, `.dirty` with
//! uncommitted changes. `greeg stats compare` tells versions apart by it.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut version = pkg.clone();
    if let Some(sha) = git(&["rev-parse", "--short=9", "HEAD"]).filter(|s| !s.is_empty()) {
        let at_tag = git(&["describe", "--tags", "--exact-match", "HEAD"])
            .is_some_and(|t| t.trim_start_matches('v') == pkg);
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        if !at_tag || dirty {
            version = format!("{pkg}+{sha}{}", if dirty { ".dirty" } else { "" });
        }
    }
    println!("cargo:rustc-env=GREEG_VERSION={version}");
    // rerun when HEAD moves or any workspace source changes, so the dirty flag is current
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        println!("cargo:rerun-if-changed={root}/.git/HEAD");
        println!("cargo:rerun-if-changed={root}/.git/index");
        for c in ["greeg", "greeg-query", "greeg-index", "greeg-lang"] {
            println!("cargo:rerun-if-changed={root}/crates/{c}/src");
        }
    }
}
