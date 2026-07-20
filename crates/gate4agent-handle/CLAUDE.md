# Crate contract

Role: handle
Owns: no domain state; only bounded ordered ingress, trusted client bindings, immutable atomic backend snapshot publication, and subscriber edge state
Exports: legacy control dispatch, scoped tool-client dispatch, trusted tool authority, snapshots, subscriptions, kernel-side drain and ordered publish primitives
Imports: gate4agent-kernel contracts, gate4agent-tool-protocol contracts, gate4agent-types, and std synchronization edges
Forbidden: mutable engine/kernel access, PTY, process, filesystem, database, network, HTTP, UI, vendor credentials
