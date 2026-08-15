//! Stamps the build's git commit into the binary.
//!
//! `0.1.0` alone cannot answer the question that actually comes up: *is the
//! thing running the thing I just built?* komo runs as two processes — the CLI
//! in `~/.cargo/bin` and the gateway in its own app bundle — installed by
//! separate steps, so they drift apart routinely, and a mismatch shows up as a
//! deserialization error rather than as a version difference. With the commit
//! in the string, `komo version` and the gateway's `/health` can simply be
//! compared.
//!
//! Absent git (a published crate, a source tarball), the stamp is `unknown` and
//! everything still builds — this must never be the reason a build fails.

use std::process::Command;

fn main() {
    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // A dirty tree is its own kind of build: the commit no longer identifies
    // what is running, and saying so is the whole point of the stamp.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    let stamp = if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    };
    println!("cargo:rustc-env=KOMO_BUILD={stamp}");

    // Re-run when HEAD moves. `.git` is a *file* in a worktree, so watch it
    // either way and let cargo ignore what does not exist.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
