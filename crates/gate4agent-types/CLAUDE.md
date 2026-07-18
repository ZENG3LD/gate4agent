# Crate contract

Role: types
Owns: no runtime state
Exports: stable agent identities, provider specifications, readiness policy, typed input preparation, and transport-neutral data contracts
Imports: std-compatible leaf dependencies and serialization
Forbidden: async runtime, locks, channels, PTY, process, filesystem, database, network, HTTP, UI, platform environment discovery
