# Crate contract

Role: engine
Owns: canonical provider registry, built-in specifications, verification state, and launch policy
Exports: validated catalog queries and shell-free launch planning
Imports: gate4agent-types
Forbidden: async runtime, locks, channels, process spawn, PATH mutation, filesystem discovery, database, network, HTTP, UI, vendor credentials
