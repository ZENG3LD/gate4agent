# Crate contract

Role: node/C2 tier wire contract (Nested Control Plane: node, wrapped by C2)
Owns: the node's bounded wire types -- inventory, spawn, sessions, worktrees, delivery -- and the harness-MCP local proxy envelope, carried as an opaque payload
Exports: NodeRequest, NodeResponse, NodeEvent, HarnessMcpLocalRequestV1, HarnessMcpLocalReplyV1, HarnessMcpOpaquePayloadV1, and their (de)serialization
Imports: gate4agent-build-stamp, gate4agent-observation-protocol, gate4agent-types
Forbidden: gate4agent-harness-api and every other gate4agent-harness-* crate -- a lower tier (node, C2) never imports the harness's crate (docs/architecture/nested-control-plane.md, Law 3 and §12); the harness-MCP request/reply this crate carries is opaque bytes plus a content-type tag, decoded only by the harness and the reviewed local helper program that spawns beside a session
