//! Grok CLI deliver channel: ACP `session/load` + `session/prompt` over an already-running grok
//! leader (`leader.sock`; a named pipe on Windows). Moved here from `mail4agent-grok`
//! (2026-10-13) so the messenger links no session channel of its own.
//!
//! Reverse requests (`session/request_permission` and other modals) are never answered: the
//! modal stays with the client that owns the session. This never spawns `grok`.

mod acp;
mod frame;
mod leader;
mod pipe;

pub use acp::{acp_request, classify_inbound, register_message, registered_is_ready, Inbound};
pub use frame::{encode_frame, MAX_FRAME_BYTES};
pub use leader::{push_into_session, wake_decrypted_room, wake_decrypted_room_blocking};
pub use pipe::{grok_home, leader_is_listening, leader_pipe_name, leader_socket, PushError};
