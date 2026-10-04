//! Reject a build that asks for the standard kit and `bare` at once.
//! Cargo cannot subtract default features, so `bare` is
//! `--no-default-features --features bare`.

fn main() {
    let kit = std::env::var_os("CARGO_FEATURE_KIT").is_some();
    let bare = std::env::var_os("CARGO_FEATURE_BARE").is_some();
    if kit && bare {
        panic!(
            "feature `bare` strips the node kit (dig2browser, mail4agent, session-restore, WireGuard); \
             build with: cargo build -p gate4agent-node --no-default-features --features bare"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
