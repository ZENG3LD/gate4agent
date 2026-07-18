# Crate contract

Role: engine
Owns: logical agent sessions, generations, lifecycle state, pending operations, snapshots, effects, and ordered events
Exports: synchronous command and observation application, snapshot publication, and egress drains
Imports: gate4agent-types
Forbidden: async runtime, locks, channels, PTY, process, filesystem, database, network, HTTP, UI, vendor credentials
