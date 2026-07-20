# Crate contract

Role: kernel
Owns: provider catalog, session engine, tool engine, deterministic phase order, and bounded ingress-to-egress step semantics
Exports: synchronous step results containing correlated reducer outcomes, control/tool effects, completions, atomic backend snapshots, and ordered control events
Imports: gate4agent-types, gate4agent-catalog, gate4agent-engine, gate4agent-tool-protocol, gate4agent-tool-engine
Forbidden: async runtime, locks, channels, PTY, process, filesystem, database, network, HTTP, UI, vendor credentials
