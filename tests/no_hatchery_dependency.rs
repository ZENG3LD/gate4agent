//! Boundary test: gate4agent is the library, hatchery is built on top of it.
//!
//! hatchery links these crates by path; the reverse edge must never exist.
//! A library that depends on the control plane built over it cannot be used
//! without that control plane, and Law 3 of the Nested Control Plane doctrine
//! (a lower tier never needs to know a higher one) stops holding by
//! construction. This test reads every manifest in the workspace and fails on
//! the first dependency line naming a `hatchery` package, whether it is a
//! direct key (`hatchery-x = ...`) or a renamed one (`package = "hatchery-x"`).
//! Prose mentioning hatchery (descriptions, comments) is not a dependency and
//! is ignored.

use std::fs;
use std::path::{Path, PathBuf};

fn manifests() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = vec![root.join("Cargo.toml")];
    let crates = fs::read_dir(root.join("crates")).expect("read crates/");
    for entry in crates {
        let manifest = entry.expect("crates/ entry").path().join("Cargo.toml");
        if manifest.is_file() {
            found.push(manifest);
        }
    }
    found
}

fn hatchery_dependency_lines(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            let key_names_hatchery = line.starts_with("hatchery") && line.contains('=');
            let renamed_to_hatchery = line.contains("package = \"hatchery");
            key_names_hatchery || renamed_to_hatchery
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_manifest_depends_on_a_hatchery_crate() {
    let manifests = manifests();
    assert!(
        manifests.len() > 1,
        "expected the root manifest plus crates/*/Cargo.toml, found {manifests:?}"
    );
    let mut offenders = Vec::new();
    for path in &manifests {
        let text = fs::read_to_string(path).expect("read manifest");
        for line in hatchery_dependency_lines(&text) {
            offenders.push(format!("{}: {line}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "gate4agent must never depend on hatchery (hatchery depends on gate4agent): {offenders:#?}"
    );
}

#[test]
fn the_detector_catches_both_dependency_shapes_and_ignores_prose() {
    let manifest = r#"
description = "the library hatchery is built on"
# hatchery-node = { path = "../x" }
hatchery-node = { path = "../../hatchery/crates/hatchery-node" }
node = { package = "hatchery-node", path = "../x" }
"#;
    assert_eq!(hatchery_dependency_lines(manifest).len(), 2);
}

/// The node and C2 belong to gate4agent, so the scan above must actually see
/// them: a boundary test that silently skipped a crate would prove nothing.
#[test]
fn the_scan_covers_the_node_and_c2_crates() {
    let scanned: Vec<String> = manifests()
        .iter()
        .filter_map(|path| path.parent()?.file_name()?.to_str().map(str::to_owned))
        .collect();
    for required in [
        "gate4agent-node",
        "gate4agent-node-protocol",
        "gate4agent-node-wire",
        "gate4agent-c2",
        "gate4agent-c2-protocol",
        "gate4agent-c2-client",
        "gate4agent-build-stamp",
    ] {
        assert!(
            scanned.iter().any(|name| name == required),
            "{required} is not covered by the hatchery-dependency scan: {scanned:?}"
        );
    }
}

fn rust_sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn hatchery_path_lines(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter(|line| {
            line.contains("use hatchery_")
                || line.contains("extern crate hatchery_")
                || line.contains(" hatchery_") && line.contains("::")
                || line.starts_with("hatchery_") && line.contains("::")
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_source_file_names_a_hatchery_crate_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for dir in ["crates", "src", "tests"] {
        rust_sources(&root.join(dir), &mut sources);
    }
    let mut offenders = Vec::new();
    for path in sources {
        if path.file_name().is_some_and(|name| name == "no_hatchery_dependency.rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in hatchery_path_lines(&text) {
            offenders.push(format!("{}: {line}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "gate4agent sources must never name a hatchery crate: {offenders:#?}"
    );
}

#[test]
fn the_source_detector_catches_use_and_path_forms_and_ignores_comments() {
    let source = "// use hatchery_node::X;
use hatchery_node::X;
let y = hatchery_c2::run();
";
    assert_eq!(hatchery_path_lines(source).len(), 2);
}

