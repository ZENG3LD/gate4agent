# Crate contract

Role: kernel
Owns: provider catalog, session engine, deterministic phase order, and bounded ingress-to-egress step semantics
Exports: synchronous step results containing command outcomes, effects, full snapshot, and ordered events
Imports: gate4agent-types, gate4agent-catalog, gate4agent-engine
Forbidden: async runtime, locks, channels, PTY, process, filesystem, database, network, HTTP, UI, vendor credentials
