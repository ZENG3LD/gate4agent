//! Computes `G4A_BUILD_STAMP`: a git content hash of the working tree that
//! contains this crate, exposed to `src/lib.rs` via `cargo:rustc-env`.
//!
//! No fallback value exists anywhere in this script. A stamp that silently
//! defaulted on a missing `git` binary or a non-repository checkout would
//! make mismatched binaries look compatible -- exactly the failure this
//! crate exists to remove. Every failure path below ends the build with one
//! sentence naming the command and the reason it failed.

use std::collections::BTreeSet;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    if let Err(message) = run() {
        panic!("{message}");
    }
}

fn run() -> Result<(), String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "gate4agent-build-stamp: CARGO_MANIFEST_DIR is not set".to_owned())?;

    // Step 1: the repository root that contains this crate.
    let toplevel = run_git(&manifest_dir, &["rev-parse", "--show-toplevel"], None)?;
    let root = PathBuf::from(toplevel.trim());
    let root_str = root.to_str().ok_or_else(|| {
        format!(
            "gate4agent-build-stamp: repository root '{}' is not valid UTF-8",
            root.display(),
        )
    })?;

    // Step 2: every tracked and untracked-but-not-ignored path, deduped and
    // sorted bytewise (a `BTreeSet<String>` gives both for free, since
    // `Ord` on `String` compares the underlying UTF-8 bytes).
    let listing = run_git_bytes(
        root_str,
        &["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        None,
    )?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for chunk in listing.split(|byte| *byte == 0) {
        if chunk.is_empty() {
            continue;
        }
        let path = String::from_utf8(chunk.to_vec()).map_err(|_| {
            "gate4agent-build-stamp: git ls-files returned a non-UTF-8 path".to_owned()
        })?;
        paths.insert(path);
    }
    if paths.is_empty() {
        return Err(format!(
            "gate4agent-build-stamp: git ls-files found no tracked or untracked-but-not-ignored files under '{}'",
            root.display(),
        ));
    }
    let paths: Vec<String> = paths.into_iter().collect();

    // Step 3: one blob id per path, from the working tree bytes, in one
    // process, in the same order the paths were fed in.
    let mut stdin_paths = String::new();
    for path in &paths {
        stdin_paths.push_str(path);
        stdin_paths.push('\n');
    }
    let hashed = run_git(
        root_str,
        &["hash-object", "--stdin-paths"],
        Some(stdin_paths.as_bytes()),
    )?;
    let blob_ids: Vec<&str> = hashed.lines().filter(|line| !line.is_empty()).collect();
    if blob_ids.len() != paths.len() {
        return Err(format!(
            "gate4agent-build-stamp: git hash-object --stdin-paths returned {} blob ids for {} input paths",
            blob_ids.len(),
            paths.len(),
        ));
    }
    for blob_id in &blob_ids {
        if !is_git_hex_id(blob_id) {
            return Err(format!(
                "gate4agent-build-stamp: git hash-object --stdin-paths returned a malformed blob id '{blob_id}'",
            ));
        }
    }

    // Step 4: the manifest is the sorted "<path>\t<blob id>\n" lines; the
    // stamp is git's own hash of that manifest.
    let mut manifest = String::new();
    for (path, blob_id) in paths.iter().zip(blob_ids.iter()) {
        manifest.push_str(path);
        manifest.push('\t');
        manifest.push_str(blob_id);
        manifest.push('\n');
    }

    let stamp = run_git(root_str, &["hash-object", "--stdin"], Some(manifest.as_bytes()))?;
    let stamp = stamp.trim().to_owned();
    if !is_git_hex_id(&stamp) {
        return Err(format!(
            "gate4agent-build-stamp: git hash-object --stdin returned a malformed stamp '{stamp}'",
        ));
    }

    println!("cargo:rustc-env=G4A_BUILD_STAMP={stamp}");
    if env::var("G4A_BUILD_STAMP_SHOW").as_deref() == Ok("1") {
        println!("cargo:warning=gate4agent build stamp: {stamp}");
    }

    // Step 5: rerun inputs. Every listed file, plus every distinct
    // directory that immediately contains one (an add/remove changes that
    // directory's listing), plus the two files that move on every commit
    // and every stage. Never the repository root itself -- that directory
    // also holds `target/`.
    let mut directories: BTreeSet<PathBuf> = BTreeSet::new();
    for path in &paths {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                directories.insert(root.join(parent));
            }
        }
    }
    for directory in &directories {
        // Cargo scans a declared directory recursively for the newest
        // mtime. A directory that holds a nested `target/` (the TUI's own
        // workspace, older per-crate builds) would make every check walk
        // gigabytes of build output, so those are left out; their tracked
        // files and their `src/`/`tests/` subdirectories are still declared
        // above, and the next edit anywhere re-lists the tree.
        if directory.join("target").is_dir() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", directory.display());
    }
    println!("cargo:rerun-if-changed={}", root.join(".git").join("HEAD").display());
    println!("cargo:rerun-if-changed={}", root.join(".git").join("index").display());

    Ok(())
}

fn is_git_hex_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Runs `git -C <dir> <args>`, optionally feeding `stdin`, and returns
/// stdout decoded as UTF-8. Never a shell -- every argument is passed to
/// `Command` directly, so a path containing spaces needs no quoting.
fn run_git(dir: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
    let bytes = run_git_bytes(dir, args, stdin)?;
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "gate4agent-build-stamp: 'git -C {dir} {}' produced non-UTF-8 output",
            args.join(" "),
        )
    })
}

fn run_git_bytes(dir: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });

    let mut child = command.spawn().map_err(|error| {
        format!(
            "gate4agent-build-stamp: failed to run 'git -C {dir} {}': {error}",
            args.join(" "),
        )
    })?;

    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().ok_or_else(|| {
            format!(
                "gate4agent-build-stamp: no stdin pipe open for 'git -C {dir} {}'",
                args.join(" "),
            )
        })?;
        pipe.write_all(bytes).map_err(|error| {
            format!(
                "gate4agent-build-stamp: failed to write stdin to 'git -C {dir} {}': {error}",
                args.join(" "),
            )
        })?;
        drop(pipe);
    }

    let output = child.wait_with_output().map_err(|error| {
        format!(
            "gate4agent-build-stamp: failed to read output of 'git -C {dir} {}': {error}",
            args.join(" "),
        )
    })?;

    if !output.status.success() {
        return Err(format!(
            "gate4agent-build-stamp: 'git -C {dir} {}' exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }

    Ok(output.stdout)
}
