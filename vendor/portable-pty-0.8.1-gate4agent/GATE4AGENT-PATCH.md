# Gate4Agent portable-pty patch

## Provenance and decision

- Upstream package: `portable-pty` 0.8.1 from crates.io.
- Crates.io checksum: `806ee80c2a03dbe1a9fb9534f8d19e4c0546b790cde8fd1fea9d6390644cb0be`.
- Upstream repository revision recorded by the package: `4afedd626dadd15d9c2929bab0e2063b54f61393`, path `pty`.
- License: MIT; the upstream `LICENSE.md` is retained beside this file.
- Capture time: 2026-08-08T22:56:04+05:00.
- Reviewer: Codex, GPT-5, primary `/root` implementation harness.
- Decision: fork and pin the already-used dependency rather than add a new PTY implementation or an unpinned Git dependency.

The package manifests, source files, platform modules, dependencies, and absence of build scripts or procedural macros were reviewed from the exact local crates.io package before execution. All retained upstream production source files are byte-identical except for `src/win/psuedocon.rs`; package identity is intentionally changed to `gate4agent-portable-pty` `0.8.1-gate4agent.1`. The fork manifest omits upstream documentation and repository URLs so it cannot claim that upstream publishes this distinct package. `Cargo.toml.orig` remains the byte-identical upstream provenance record. Upstream examples, package-local lock data, and cargo registry metadata are not vendored.

## Gate4Agent change

Microsoft declares `CreatePseudoConsole`, `ResizePseudoConsole`, and `ClosePseudoConsole` as `WINAPI`. The upstream `shared_library!` macro stores them as `unsafe extern fn`, which uses the C ABI and corrupts the stack on 32-bit Windows. The Gate4Agent fork keeps dynamic `kernel32.dll`/sideloaded `conpty.dll` lookup but types the three symbols as `unsafe extern "system" fn`.

The loader is also fallible. Windows versions without ConPTY now return the existing PTY creation error instead of panicking during lazy initialization.

No Unix or public portable-pty API behavior is changed.

The fork is a direct, exact Gate4Agent dependency rather than a root Cargo patch. A published Gate4Agent package must therefore publish this fork first; it cannot silently fall back to upstream `portable-pty`.

## Verification and residual risk

- Windows i686 and x86_64 ConPTY canaries passed create, spawn, input, resize, drain, and close.
- The controlled i686 PTY runtime lifecycle and i686/x86_64 Node PTY fixture E2E passed.
- i686 Node and C2 binaries compiled from the locked graph.
- Retained Unix source files were hash-compared to upstream; native Unix behavior was not rerun in this Windows slice.
- Claude, Codex, and Kimi executable availability on 32-bit-only Windows remains vendor-controlled and is not claimed by this patch.
- A future upstream update requires a new static delta review and ABI regression run.
