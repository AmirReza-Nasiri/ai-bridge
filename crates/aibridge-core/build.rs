//! Embeds build-time provenance (git short SHA + commit date, with a `-dirty`
//! suffix for uncommitted local builds) into the binary as compile-time env vars.
//! Falls back to "unknown" when git isn't available (e.g. building from a tarball
//! in CI). The semver version itself comes from `CARGO_PKG_VERSION` (Cargo.toml).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date =
        git(&["log", "-1", "--format=%cd", "--date=short"]).unwrap_or_else(|| "unknown".into());
    // `--porcelain` lists tracked changes + non-ignored untracked; empty == clean.
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let sha = if dirty { format!("{sha}-dirty") } else { sha };

    println!("cargo:rustc-env=AIBRIDGE_GIT_SHA={sha}");
    println!("cargo:rustc-env=AIBRIDGE_BUILD_DATE={date}");

    // Refresh the stamp when HEAD / refs / staged state move (best-effort; path is
    // relative to this crate dir = <repo>/crates/aibridge-core). `index` catches
    // staging changes that flip the dirty flag; `packed-refs` catches packed refs.
    if std::path::Path::new("../../.git/HEAD").exists() {
        for f in ["HEAD", "index", "packed-refs"] {
            println!("cargo:rerun-if-changed=../../.git/{f}");
        }
    }
}
