//! One build stamp for every wire handshake in this repository tree.
//!
//! `BUILD_STAMP` is a content hash of the working tree at compile time
//! (see `build.rs`): it is git's own blob hashing applied to every tracked
//! and untracked-but-not-ignored file, folded into a single 40-hex-digit
//! id. Two binaries built from byte-identical checkouts get the same
//! stamp; two binaries built from checkouts that differ by even one byte
//! almost certainly do not. It answers exactly one question at connection
//! time -- "did this peer come from the same tree I did?" -- compared for
//! exact equality, nothing else.
//!
//! It is NOT a storage-format version. A checkpoint, database schema, or
//! any other persisted record that needs forward migration across
//! releases gets its own explicit integer, chosen and bumped by hand, as
//! today. Wiring `BUILD_STAMP` into a persisted header would make every
//! unrelated source change anywhere in the tree look like a storage
//! migration.
pub const BUILD_STAMP: &str = env!("G4A_BUILD_STAMP");

#[cfg(test)]
mod tests {
    use super::BUILD_STAMP;

    #[test]
    fn build_stamp_is_a_forty_hex_digit_git_hash() {
        assert_eq!(BUILD_STAMP.len(), 40);
        assert!(BUILD_STAMP.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
