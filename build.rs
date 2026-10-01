//! Embeds the commit this binary was built from as `MCC_COMMIT`, which
//! `/api/version` and the operator pages show.
//!
//! `MCC_COMMIT` in the build environment wins, for a build with no `.git` to ask
//! (a source tarball, a `cross` container that cannot read the repository).
//! Without either it is `unknown` rather than a build failure.
//!
//! Only the commit, not a `-dirty` flag: telling a dirty tree apart would mean
//! rerunning this script on every change to any file, and a deployed binary is
//! built from a commit anyway.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-env-changed=MCC_COMMIT");

    let commit = std::env::var("MCC_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "--short=7", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());

    // Rebuild when HEAD moves. Paths come from git rather than `.git/...`
    // because in a submodule `.git` is a file pointing elsewhere. Only existing
    // ones: cargo reruns the script on every build for a path that is missing.
    let mut watched = vec![git(&["rev-parse", "--git-path", "HEAD"])];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(git(&["rev-parse", "--git-path", &branch]));
    }
    watched.push(git(&["rev-parse", "--git-path", "packed-refs"]));
    for path in watched.into_iter().flatten() {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    println!("cargo:rustc-env=MCC_COMMIT={commit}");
}
