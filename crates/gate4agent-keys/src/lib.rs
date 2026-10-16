//! g4a's keystore layer. g4a CONSUMES keys; the crypto is `tesserax-keytree`.
//!
//! Which transports a node or session may use follows from the grants its key holds:
//! a grant's namespace names the transport (`node.uplink`, `node.env_ctl`, `node.pty_attach`,
//! `session.*`). m4a grants (`m4a.*`) belong to m4a and pass through untouched.
//!
//! - **Standalone**: [`Authority::Own`] — g4a holds its own root and issues node/session caps.
//! - **Under an orchestrator**: [`Authority::Delegated`] — no root here. Keys stay in the
//!   orchestrator's keystore; g4a gets attenuated capabilities and process-bound handles only.

use std::collections::BTreeSet;

pub use tesserax_keytree as keytree;

/// Grants g4a understands.
pub mod grants {
    pub const NODE_UPLINK: &str = "node.uplink";
    pub const NODE_ENV_CTL: &str = "node.env_ctl";
    pub const NODE_PTY_ATTACH: &str = "node.pty_attach";
    pub const SESSION_RUN_CLOSE: &str = "session.run_close";
    pub const SESSION_SPAWN_CHILD: &str = "session.spawn_child";
    /// Earlier un-namespaced names, accepted as aliases.
    pub const LEGACY: &[(&str, &str)] = &[("run_close", SESSION_RUN_CLOSE), ("spawn_child", SESSION_SPAWN_CHILD)];

    pub fn canonical(name: &str) -> &str {
        LEGACY.iter().find(|(l, _)| *l == name).map(|(_, g)| *g).unwrap_or(name)
    }
    /// Transport (namespace) a grant unlocks.
    pub fn transport(name: &str) -> &str {
        canonical(name).split('.').next().unwrap_or("")
    }
    /// Grants that need the human per operation.
    pub fn needs_human(name: &str) -> bool {
        matches!(canonical(name), NODE_ENV_CTL | SESSION_SPAWN_CHILD)
    }
}

/// Transports a grant set makes available, e.g. `{"node", "m4a"}`.
pub fn transports(granted: &BTreeSet<String>) -> BTreeSet<String> {
    granted.iter().map(|g| grants::transport(g).to_owned()).filter(|t| !t.is_empty()).collect()
}

/// Who is the root of g4a's keys.
pub enum Authority {
    Own(keytree::Authority),
    Delegated,
}

impl Authority {
    pub fn standalone() -> Self {
        Self::Own(keytree::Authority::generate())
    }
    pub fn is_delegated(&self) -> bool {
        matches!(self, Self::Delegated)
    }

    /// Standalone only: capability for `subject` (node or session id) with g4a grants.
    /// Grants needing a human require `human_approved`.
    pub fn issue(&self, subject: &str, granted: &BTreeSet<String>, human_approved: bool) -> Result<keytree::Issued, keytree::Error> {
        let Self::Own(root) = self else { return Err(keytree::Error::Denied) };
        let g: BTreeSet<String> = granted.iter().map(|x| grants::canonical(x).to_owned()).collect();
        if g.iter().any(|x| !matches!(grants::transport(x), "node" | "session")) {
            return Err(keytree::Error::Denied);
        }
        if !human_approved && g.iter().any(|x| grants::needs_human(x)) {
            return Err(keytree::Error::Denied);
        }
        root.issue(&[("subject", subject)], subject, &g)
    }

    /// Standalone only: subject the capability grants `op` to.
    pub fn verify(&self, token: &str, op: &str) -> Result<String, keytree::Error> {
        let Self::Own(root) = self else { return Err(keytree::Error::Denied) };
        let v = root.verify(token, grants::canonical(op), "subject", &[])?;
        v.facts.get("subject").cloned().ok_or(keytree::Error::BadToken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(x: &[&str]) -> BTreeSet<String> {
        x.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn transports_follow_grants() {
        assert_eq!(transports(&set(&["node.uplink", "m4a.send_as", "run_close"])), set(&["m4a", "node", "session"]));
    }

    #[test]
    fn standalone_issue_verify_and_human_gate() {
        let a = Authority::standalone();
        let t = a.issue("n1", &set(&[grants::NODE_UPLINK]), false).unwrap();
        assert_eq!(a.verify(&t.token, grants::NODE_UPLINK).unwrap(), "n1");
        assert!(a.verify(&t.token, grants::NODE_ENV_CTL).is_err());
        assert!(a.issue("n1", &set(&[grants::NODE_ENV_CTL]), false).is_err());
        assert!(a.issue("n1", &set(&[grants::NODE_ENV_CTL]), true).is_ok());
        assert!(a.issue("n1", &set(&["m4a.send_as"]), true).is_err(), "m4a grants are not g4a's to issue");
        assert!(Authority::Delegated.issue("n1", &set(&[]), true).is_err());
    }
}
