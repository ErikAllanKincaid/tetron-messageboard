//! Build script: stamp the git short SHA into the binary so builds are
//! identifiable. `tetron-messageboard --version`/`version` surfaces it. Same
//! pattern as tetron core's own `build.rs` and every sibling addon's copy.
//!
//! Falls back to `unknown` when git is unavailable (e.g. a source tarball
//! build outside a checkout), so the build never fails for lack of a `.git`
//! dir.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_SHA={sha}");

    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=.git/packed-refs");
}
