//! Standard node kit: dig2browser, mail4agent, the four session-restore
//! crates, and kernel WireGuard. Compiled only with feature `kit`
//! (the default). Feature `bare` does not compile this module.

/// The four cores, in owner order.
pub const CORE_NAMES: [&str; 4] = ["dig2browser", "mail4agent", "session-restore", "wireguard"];

pub fn linked_core_names() -> &'static [&'static str] {
    &CORE_NAMES
}

/// Touches a real symbol in each core so the default node binary links them.
/// Does not start a browser, a mailbox, a provider CLI, or a tunnel, and
/// does not read a secret.
pub fn force_link() -> usize {
    let dig2 = dig2browser::browser_stream::LOCAL_PAGE_HTML.len();
    let mail: fn() -> Result<(), mail4agent::MainError> = mail4agent::main;
    let claude = std::mem::size_of::<claude_session_restore::transcript::SessionEvent>();
    let codex = codex_session_restore::MAX_HEAD_BYTES;
    let grok: fn() = grok_session_restore::main;
    let kimi: fn() = kimi_session_restore::main;
    let wireguard: fn(
        &crate::kit_wireguard::NodeWgPeerConfig,
    ) -> Result<std::net::IpAddr, crate::kit_wireguard::NodeWgPeerError> =
        crate::kit_wireguard::bring_up_node_wireguard;
    dig2.wrapping_add(claude)
        .wrapping_add(codex)
        .wrapping_add(mail as usize)
        .wrapping_add(grok as usize)
        .wrapping_add(kimi as usize)
        .wrapping_add(wireguard as usize)
        .wrapping_add(crate::kit_wireguard::LINK_TAG.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_names_the_four_cores() {
        assert_eq!(
            linked_core_names(),
            ["dig2browser", "mail4agent", "session-restore", "wireguard"]
        );
        assert!(force_link() > 0);
    }
}
