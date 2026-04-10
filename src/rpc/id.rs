//! Monotonically increasing request ID generator.

use std::sync::atomic::{AtomicU64, Ordering};

use super::message::RpcId;

/// Thread-safe monotonically increasing request ID generator.
///
/// Starts at 1. 0 is conventionally reserved for `initialize` by the ACP spec.
pub struct IdGen(AtomicU64);

impl IdGen {
    /// Create a new generator starting at 1.
    pub const fn new() -> Self {
        Self(AtomicU64::new(1))
    }

    /// Get the next unique request ID.
    pub fn next(&self) -> RpcId {
        RpcId::Number(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_one() {
        let gen = IdGen::new();
        assert_eq!(gen.next(), RpcId::Number(1));
    }

    #[test]
    fn monotonically_increasing() {
        let gen = IdGen::new();
        let a = gen.next();
        let b = gen.next();
        let c = gen.next();
        assert_eq!(a, RpcId::Number(1));
        assert_eq!(b, RpcId::Number(2));
        assert_eq!(c, RpcId::Number(3));
    }
}
