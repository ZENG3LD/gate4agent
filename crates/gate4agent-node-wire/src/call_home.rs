//! The call-home preface: one frame, sent by a node that dials the relay
//! instead of waiting to be dialled.
//!
//! Everything else on this wire is opened by the operator, so the operator
//! always knows whose socket it holds. A node with no reachable address
//! cannot be dialled at all, so it connects out -- and then the relay is
//! holding an accepted socket with no idea which node is on the far end,
//! and no way to guess, because verifying the node's own proof requires
//! that node's access token.
//!
//! So the node names itself first, and the handshake that follows is
//! byte-for-byte the one a dialled connection runs. See
//! [`NodeCallHomeAnnounce`]'s own doc comment for why naming yourself is
//! not authentication and grants nothing.

use std::time::Duration;

use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, FrameError,
    NodeCallHomeAnnounce, NodeId, BUILD_STAMP, MAX_NODE_HELLO_FRAME_BYTES,
};
use tokio::io::{AsyncRead, AsyncWrite};

/// How long the relay will wait for a freshly accepted socket to name its
/// node before dropping it. Deliberately short and deliberately its own
/// constant rather than the handshake's: an accepted socket that has not
/// yet said anything is the cheapest thing in the world to open and the
/// only cost of holding it is ours, so it gets less patience than a peer
/// that has already identified itself.
const ANNOUNCE_TIMEOUT_MS: u64 = 2_000;

/// Why a call-home preface could not be read.
#[derive(Debug)]
pub enum CallHomeAnnounceError {
    /// The socket said nothing, or not enough, before the deadline.
    TimedOut,
    /// The bytes were not a well-formed preface.
    Frame(FrameError),
    /// A preface arrived, from a peer built from a different tree than
    /// this binary. Reported separately from `Frame` because it is the one
    /// failure here that is a deployment mistake rather than a hostile or
    /// broken peer, and it should read as one in a log.
    BuildStamp { announced: String },
    /// The name is not a syntactically valid node id. Checked here rather
    /// than left to the lookup so a malformed name is refused at the door
    /// with a reason, instead of becoming an indistinguishable "no such
    /// node" a moment later.
    InvalidNodeId,
}

impl std::fmt::Display for CallHomeAnnounceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => write!(formatter, "call-home peer did not announce itself in time"),
            Self::Frame(error) => write!(formatter, "call-home announce frame is invalid: {error}"),
            Self::BuildStamp { announced } => write!(
                formatter,
                "build stamp mismatch: local={BUILD_STAMP} remote={announced}",
            ),
            Self::InvalidNodeId => write!(formatter, "call-home announce carried an invalid node id"),
        }
    }
}

impl std::error::Error for CallHomeAnnounceError {}

/// Node side: name yourself on a socket you just opened.
pub async fn write_call_home_announce<W>(
    writer: &mut W,
    node_id: &NodeId,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    write_json_frame_limited(
        writer,
        &NodeCallHomeAnnounce::new(node_id.as_str()),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
}

/// Relay side: find out whose socket this is.
///
/// Returns the announced id parsed as a [`NodeId`]. It is a claim, not a
/// credential: the caller looks up that node's configured access token and
/// the handshake decides whether the claim was true.
pub async fn read_call_home_announce<R>(reader: &mut R) -> Result<NodeId, CallHomeAnnounceError>
where
    R: AsyncRead + Unpin,
{
    let announce: NodeCallHomeAnnounce = tokio::time::timeout(
        Duration::from_millis(ANNOUNCE_TIMEOUT_MS),
        read_json_frame_limited_body_timeout(
            reader,
            MAX_NODE_HELLO_FRAME_BYTES,
            Duration::from_millis(ANNOUNCE_TIMEOUT_MS),
        ),
    )
    .await
    .map_err(|_| CallHomeAnnounceError::TimedOut)?
    .map_err(CallHomeAnnounceError::Frame)?;
    if announce.build_stamp != BUILD_STAMP {
        return Err(CallHomeAnnounceError::BuildStamp {
            announced: announce.build_stamp,
        });
    }
    NodeId::new(announce.node_id).map_err(|_| CallHomeAnnounceError::InvalidNodeId)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preface round-trips over a plain byte pipe, which is the whole
    /// contract: it is written by whoever opened the socket and read by
    /// whoever accepted it, over any stream at all.
    #[tokio::test]
    async fn a_node_names_itself_and_the_relay_reads_the_name() {
        let node_id = NodeId::new("opbox-windows-x86-64-1d67e837f8fa").unwrap();
        let mut wire = Vec::new();
        write_call_home_announce(&mut wire, &node_id).await.unwrap();
        let read = read_call_home_announce(&mut wire.as_slice()).await.unwrap();
        assert_eq!(read, node_id);
    }

    /// A peer built from a different tree is told which mismatch it is,
    /// not handed a generic parse failure -- this is the one error here
    /// that means "your deployment is mixed", and it has to read that way.
    #[tokio::test]
    async fn a_build_stamp_mismatch_names_itself_rather_than_looking_like_garbage() {
        let foreign_stamp = "f".repeat(BUILD_STAMP.len());
        let mut wire = Vec::new();
        write_json_frame_limited(
            &mut wire,
            &NodeCallHomeAnnounce {
                build_stamp: foreign_stamp.clone(),
                node_id: "opbox-windows-x86-64-1d67e837f8fa".to_owned(),
            },
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await
        .unwrap();
        let error = read_call_home_announce(&mut wire.as_slice()).await.unwrap_err();
        assert!(
            matches!(&error, CallHomeAnnounceError::BuildStamp { announced } if *announced == foreign_stamp),
            "expected a named build stamp mismatch, got {error:?}",
        );
        assert_eq!(
            error.to_string(),
            format!("build stamp mismatch: local={BUILD_STAMP} remote={foreign_stamp}"),
        );
    }

    /// Silence costs the relay a bounded wait and nothing else.
    #[tokio::test]
    async fn a_socket_that_says_nothing_is_dropped_rather_than_held() {
        let (client, mut server) = tokio::io::duplex(64);
        // `client` is kept alive and never written to: the peer connected
        // and then said nothing, which is exactly the cheap-to-open,
        // expensive-to-hold case the deadline exists for.
        let error = read_call_home_announce(&mut server).await.unwrap_err();
        drop(client);
        assert!(
            matches!(error, CallHomeAnnounceError::TimedOut),
            "expected the announce deadline to fire, got {error:?}",
        );
    }
}
