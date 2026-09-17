//! Boundary test for the Nested Control Plane doctrine's Law 3
//! (`docs/architecture/nested-control-plane.md`): a lower tier -- `node`,
//! wrapped by `c2` -- must never import a higher tier's crate, here the
//! harness's `gate4agent-harness-api`. See this crate's own `CLAUDE.md`
//! `Forbidden:` line.
//!
//! This does not re-derive the compiler's own guarantee -- Rust cannot
//! resolve a `gate4agent_harness_api::…` path unless the crate is listed as
//! a dependency, so no source file in this crate can reintroduce the old
//! re-export without also touching `Cargo.toml` first. What this test
//! catches is exactly that first step: `Cargo.toml` growing a
//! `gate4agent-harness-*` dependency line back in, whether or not anyone
//! goes on to write a `use` for it. It would not catch a dependency
//! reintroduced under a renamed path that avoids the `gate4agent-harness`
//! prefix entirely -- an unlikely and immediately visible kind of evasion,
//! not a silent one.
#[test]
fn manifest_never_reacquires_a_harness_dependency() {
    let manifest = include_str!("../Cargo.toml");
    assert!(
        !manifest.contains("gate4agent-harness"),
        "gate4agent-node-protocol/Cargo.toml must never depend on a \
         gate4agent-harness-* crate -- see this crate's CLAUDE.md \
         `Forbidden:` line and docs/architecture/nested-control-plane.md",
    );
}
