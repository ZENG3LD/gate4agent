# Crate contract

Role: pty backend
Owns: PTY creation and child-process lifecycle (Windows ConPTY, unix posix_openpt)
Exports: `PtySize`, `CommandBuilder`, `ExitStatus`, `Error`, traits `PtySystem`/`MasterPty`/`SlavePty`/`Child`/`ChildKiller`, `native_pty_system()`
Imports: std only — no external crates on any platform
Forbidden: winapi/windows-sys/libc/nix/anyhow crates, async runtime, panics on ordinary
  OS failure (typed `Error`/`io::Error` only), `#[allow(dead_code)]` and friends
