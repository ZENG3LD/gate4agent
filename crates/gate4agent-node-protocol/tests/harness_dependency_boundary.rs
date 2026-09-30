//! Boundary test for the Nested Control Plane doctrine's Law 3
//! (`docs/architecture/nested-control-plane.md`): a lower tier -- `node`,
//! wrapped by `c2` -- must never import a higher tier's crate, here anything
//! from `hatchery` (harness, observation, tui). See this crate's own
//! `CLAUDE.md` `Forbidden:` line.
//!
//! Rust cannot resolve a `hatchery_*::...` path unless the crate is listed as
//! a dependency, so what this test catches is the first step: `Cargo.toml`
//! growing a `hatchery-*` dependency line back in.
#[test]
fn manifest_never_reacquires_a_hatchery_dependency() {
    let manifest = include_str!("../Cargo.toml");
    let offenders: Vec<&str> = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            (line.starts_with("hatchery") && line.contains('='))
                || line.contains("package = \"hatchery")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "gate4agent-node-protocol/Cargo.toml must never depend on a hatchery-* crate; \n         see this crate's CLAUDE.md Forbidden line: {offenders:?}",
    );
}
