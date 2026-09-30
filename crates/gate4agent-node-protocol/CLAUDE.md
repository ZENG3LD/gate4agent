# Crate contract

Role: node/C2 tier wire contract (Nested Control Plane: node, wrapped by C2)
Owns: the node's bounded wire types -- inventory, spawn, sessions, worktrees, delivery -- the opaque correlation ids of the event stream (`correlation`), and the harness-MCP local proxy envelope, carried as an opaque payload
Exports: NodeRequest, NodeResponse, NodeEvent, HarnessMcpLocalRequestV1, HarnessMcpLocalReplyV1, HarnessMcpOpaquePayloadV1, and their (de)serialization
Imports: gate4agent-build-stamp, gate4agent-types
Forbidden: any hatchery-* crate (harness, observation, tui) -- a lower tier (node, C2) never imports a higher one (docs/architecture/nested-control-plane.md, Law 3 and §12); no observation/telemetry vocabulary lives here, hatchery derives its own from `NodeEvent::Control` and the agent stream; the harness-MCP request/reply this crate carries is opaque bytes plus a content-type tag, decoded only by the harness and the reviewed local helper program that spawns beside a session
