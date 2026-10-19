// Hands the build's git state to `--version` (distribution design section 3.5):
// a tagged, clean build prints the bare version; any other build adds the commit
// id as semver build metadata, `+<commit>`, and `.dirty` for uncommitted
// changes. Mirrors the engine's generateBuildInfo task. Without git (a source
// tarball) the commit is `dev`.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// A tracked file whose content differs from HEAD. A mode-only change shows
// `0 0` and does not count; a binary change shows `-` and does.
fn content_differs_from_head() -> bool {
    let Some(numstat) = git(&["diff", "HEAD", "--numstat"]) else { return false };
    numstat.lines().any(|line| {
        let mut cols = line.split('\t');
        let (Some(added), Some(deleted)) = (cols.next(), cols.next()) else { return false };
        added == "-" || deleted == "-" || added.parse::<u64>().unwrap_or(0) > 0 || deleted.parse::<u64>().unwrap_or(0) > 0
    })
}

// `v<version>` or `<version>`, with an optional `-pkg<N>` packaging revision.
fn names_release(tag: &str, version: &str) -> bool {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let Some(rest) = tag.strip_prefix(version) else { return false };
    match rest.strip_prefix("-pkg") {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => rest.is_empty(),
    }
}

fn watch(path: &str) {
    if Path::new(path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let commit = git(&["rev-parse", "--short", "HEAD"]).filter(|c| !c.is_empty());
    let dirty = commit.is_some()
        && (content_differs_from_head()
            || git(&["ls-files", "--others", "--exclude-standard"]).is_some_and(|files| !files.is_empty()));
    let tagged = commit.is_some()
        && !dirty
        && git(&["tag", "--points-at", "HEAD"]).is_some_and(|tags| tags.lines().any(|t| names_release(t, &version)));

    println!("cargo:rustc-env=UNIDRIVE_MOUNT_COMMIT={}", commit.as_deref().unwrap_or("dev"));
    println!("cargo:rustc-env=UNIDRIVE_MOUNT_DIRTY={dirty}");
    println!("cargo:rustc-env=UNIDRIVE_MOUNT_TAGGED={tagged}");

    // A new commit or tag changes the answer without touching a source file.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        watch(&format!("{dir}/HEAD"));
        watch(&format!("{dir}/index"));
    }
    if let Some(dir) = git(&["rev-parse", "--git-common-dir"]) {
        watch(&format!("{dir}/refs"));
        watch(&format!("{dir}/packed-refs"));
    }
}
