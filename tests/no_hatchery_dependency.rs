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
