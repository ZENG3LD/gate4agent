//! The deliver API must never pull in the terminal capability (PTY / shell / runtime / node-core),
//! so a host can grant "deliver" without granting "terminal".

#[test]
fn deliver_crate_does_not_depend_on_terminal_crates() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    for banned in ["gate4agent-pty", "gate4agent-shell-", "gate4agent-runtime-native", "gate4agent-node-core", "portable-pty"] {
        assert!(!manifest.contains(banned), "gate4agent-wake must not depend on {banned}");
    }
}
