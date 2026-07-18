# Crate contract

Role: handle
Owns: no domain state; only bounded ingress, immutable snapshot publication, and subscriber edge state
Exports: dispatch, snapshot, subscribe, kernel-side drain and publish primitives
Imports: gate4agent-types and std synchronization edges
Forbidden: mutable engine/kernel access, PTY, process, filesystem, database, network, HTTP, UI, vendor credentials
