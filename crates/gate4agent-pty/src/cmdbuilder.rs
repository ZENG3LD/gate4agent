//! `CommandBuilder` — prepares a command to be spawned into a pty.
//!
//! The interface is intentionally similar to `std::process::Command`, but the
//! actual spawn plumbing lives in the platform backends (`windows::conpty`,
//! `unix::spawn`) because neither ConPTY nor a raw fork/exec pty child can be
//! produced through `std::process::Command`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// A single environment variable slot. `preferred_key` retains the caller's
/// original casing for the environment block; the map's own key is the
/// case-folded lookup key (folded only on Windows, where env var names are
/// case-insensitive).
#[derive(Clone, Debug)]
struct EnvEntry {
    preferred_key: OsString,
    value: OsString,
}

fn map_key(key: OsString) -> OsString {
    #[cfg(windows)]
    {
        match key.to_str() {
            Some(s) => OsString::from(s.to_lowercase()),
            None => key,
        }
    }
    #[cfg(unix)]
    {
        key
    }
}

/// The command's starting environment is the current process's environment.
/// Callers that want an isolated child environment call `env_clear()` before
/// adding variables back with `env()` — this crate does not itself consult
/// the Windows registry or the unix passwd database for a "live" base
/// environment, since every gate4agent call site clears and rebuilds the
/// environment explicitly before spawning.
fn base_env() -> BTreeMap<OsString, EnvEntry> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                map_key(key.clone()),
                EnvEntry {
                    preferred_key: key,
                    value,
                },
            )
        })
        .collect()
}

/// `CommandBuilder` is used to prepare a command to be spawned into a pty.
#[derive(Clone, Debug)]
pub struct CommandBuilder {
    args: Vec<OsString>,
    envs: BTreeMap<OsString, EnvEntry>,
    cwd: Option<OsString>,
}

impl CommandBuilder {
    /// Create a new builder instance with argv[0] set to the specified
    /// program.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            args: vec![program.as_ref().to_owned()],
            envs: base_env(),
            cwd: None,
        }
    }

    /// Create a new builder instance from a pre-built argument vector.
    /// `args[0]` is the program to execute.
    pub fn from_argv(args: Vec<OsString>) -> Self {
        Self {
            args,
            envs: base_env(),
            cwd: None,
        }
    }

    /// Append an argument to the current command line.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        self.args.push(arg.as_ref().to_owned());
    }

    /// Append a sequence of arguments to the current command line.
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    /// The full argv, including argv[0].
    pub fn get_argv(&self) -> &Vec<OsString> {
        &self.args
    }

    /// Override the value of an environment variable.
    pub fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let key: OsString = key.as_ref().to_owned();
        let value: OsString = value.as_ref().to_owned();
        self.envs.insert(
            map_key(key.clone()),
            EnvEntry {
                preferred_key: key,
                value,
            },
        );
    }

    /// Remove an environment variable, if present.
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) {
        let key: OsString = key.as_ref().to_owned();
        self.envs.remove(&map_key(key));
    }

    /// Remove every environment variable. Callers typically follow this with
    /// a curated set of `env()` calls to build an isolated child environment.
    pub fn env_clear(&mut self) {
        self.envs.clear();
    }

    /// Look up a configured environment variable by name (case-insensitive
    /// on Windows, case-sensitive on unix).
    pub fn get_env<K: AsRef<OsStr>>(&self, key: K) -> Option<&OsStr> {
        let key: OsString = key.as_ref().to_owned();
        self.envs
            .get(&map_key(key))
            .map(|entry| entry.value.as_os_str())
    }

    /// Set the working directory the child process starts in.
    pub fn cwd<D: AsRef<OsStr>>(&mut self, dir: D) {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    /// The configured working directory, if any.
    pub fn get_cwd(&self) -> Option<&OsString> {
        self.cwd.as_ref()
    }

    /// Iterate over the full configured environment as UTF-8 `(key, value)`
    /// pairs. Entries whose key or value is not valid UTF-8 are skipped.
    pub fn iter_full_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(|entry| {
            let key = entry.preferred_key.to_str()?;
            let value = entry.value.to_str()?;
            Some((key, value))
        })
    }

    fn program(&self) -> crate::Result<&OsStr> {
        self.args
            .first()
            .map(OsString::as_os_str)
            .ok_or_else(|| crate::Error::Invalid("CommandBuilder has no program (argv[0] is empty)".into()))
    }
}

// ---------------------------------------------------------------------------
// Windows: command-line quoting, PATH/PATHEXT search, environment block, cwd.
// ---------------------------------------------------------------------------

#[cfg(windows)]
impl CommandBuilder {
    fn search_path(&self, exe: &OsStr) -> OsString {
        // No let-else anywhere in this crate: the Linux verification boxes
        // run rustc 1.63 (see unix/mod.rs floor note).
        let path = match self.get_env("PATH") {
            Some(path) => path,
            None => return exe.to_owned(),
        };
        let extensions = self.get_env("PATHEXT").unwrap_or(OsStr::new(".EXE"));
        for dir in std::env::split_paths(path) {
            // Check for exactly the caller's string in this path dir.
            let candidate = dir.join(exe);
            if candidate.exists() {
                return candidate.into_os_string();
            }
            // Otherwise try tacking on some extensions. This replaces the
            // extension in the caller-specified path, so it is potentially
            // wrong for a name that already has a (different) extension.
            for ext in std::env::split_paths(extensions) {
                let ext = match ext.to_str() {
                    Some(ext) => ext,
                    None => continue,
                };
                let ext = match ext.strip_prefix('.').filter(|e| !e.is_empty()) {
                    Some(ext) => ext,
                    None => continue,
                };
                let candidate = dir.join(exe).with_extension(ext);
                if candidate.exists() {
                    return candidate.into_os_string();
                }
            }
        }
        exe.to_owned()
    }

    /// The wide, nul-terminated working directory for `CreateProcessW`, or
    /// `None` to inherit the parent's current directory.
    pub(crate) fn current_directory(&self) -> Option<Vec<u16>> {
        use std::path::Path;

        let dir = self
            .cwd
            .as_deref()
            .filter(|path| Path::new(path).is_dir())?;

        let mut wide = Vec::new();
        if Path::new(dir).is_relative() {
            if let Ok(absolute) = std::env::current_dir() {
                wide.extend(absolute.join(dir).as_os_str().encode_wide());
            } else {
                wide.extend(dir.encode_wide());
            }
        } else {
            wide.extend(dir.encode_wide());
        }
        wide.push(0);
        Some(wide)
    }

    /// A double-nul-terminated `CREATE_UNICODE_ENVIRONMENT` block for
    /// `CreateProcessW`.
    pub(crate) fn environment_block(&self) -> Vec<u16> {
        let mut block = Vec::new();
        for entry in self.envs.values() {
            block.extend(entry.preferred_key.encode_wide());
            block.push(b'=' as u16);
            block.extend(entry.value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    /// Returns `(module_name, command_line)`, both nul-terminated UTF-16,
    /// ready for `CreateProcessW`'s `lpApplicationName`/`lpCommandLine`.
    pub(crate) fn cmdline(&self) -> crate::Result<(Vec<u16>, Vec<u16>)> {
        let program = self.program()?;
        let resolved = self.search_path(program);

        let mut cmdline = Vec::<u16>::new();
        Self::append_quoted(&resolved, &mut cmdline);

        let mut exe: Vec<u16> = resolved.encode_wide().collect();
        exe.push(0);

        for arg in self.args.iter().skip(1) {
            cmdline.push(b' ' as u16);
            if arg.encode_wide().any(|c| c == 0) {
                return Err(crate::Error::Invalid(format!(
                    "invalid encoding for command line argument {arg:?}: embedded NUL"
                )));
            }
            Self::append_quoted(arg, &mut cmdline);
        }
        cmdline.push(0);
        Ok((exe, cmdline))
    }

    // Ported from the wezterm/portable-pty fork's `append_quoted`, itself a
    // translation of Microsoft's `ArgvQuote` (see
    // https://learn.microsoft.com/en-us/archive/blogs/twistylittlepassagesallalike/everyone-quotes-command-line-arguments-the-wrong-way).
    // Windows has no shell-independent argv; the child re-parses the single
    // `lpCommandLine` string itself, and this is the exact backslash/quote
    // counting algorithm the Microsoft C runtime uses to reverse it.
    fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
        if !arg.is_empty()
            && !arg.encode_wide().any(|c| {
                c == b' ' as u16
                    || c == b'\t' as u16
                    || c == b'\n' as u16
                    || c == 0x0b // \x0b vertical tab
                    || c == b'"' as u16
            })
        {
            cmdline.extend(arg.encode_wide());
            return;
        }
        cmdline.push(b'"' as u16);

        let arg: Vec<u16> = arg.encode_wide().collect();
        let mut i = 0;
        while i < arg.len() {
            let mut num_backslashes = 0;
            while i < arg.len() && arg[i] == b'\\' as u16 {
                i += 1;
                num_backslashes += 1;
            }

            if i == arg.len() {
                for _ in 0..num_backslashes * 2 {
                    cmdline.push(b'\\' as u16);
                }
                break;
            } else if arg[i] == b'"' as u16 {
                for _ in 0..num_backslashes * 2 + 1 {
                    cmdline.push(b'\\' as u16);
                }
                cmdline.push(arg[i]);
            } else {
                for _ in 0..num_backslashes {
                    cmdline.push(b'\\' as u16);
                }
                cmdline.push(arg[i]);
            }
            i += 1;
        }
        cmdline.push(b'"' as u16);
    }
}

// ---------------------------------------------------------------------------
// Unix: PATH search and CString argv/envp/cwd construction for fork+exec.
// ---------------------------------------------------------------------------

#[cfg(unix)]
impl CommandBuilder {
    /// Resolve argv[0] to a path suitable for `execve`. A name containing a
    /// `/` is used as-is (matching `execvp`'s own rule); a bare name is
    /// searched for in `$PATH`.
    ///
    /// This duplicates part of what `execvp` would do, rather than calling
    /// `execvp` itself, because `execvp`'s PATH search reads the *calling
    /// process's real* `PATH`, not the `envp` this builder constructs — a
    /// caller that overrides `PATH` via `env("PATH", ...)` would otherwise
    /// have that override silently ignored during the executable search.
    fn resolve_program(&self) -> crate::Result<OsString> {
        let program = self.program()?;
        if program.as_bytes().contains(&b'/') {
            return Ok(program.to_owned());
        }
        if let Some(path) = self.get_env("PATH") {
            for dir in std::env::split_paths(path) {
                let candidate = dir.join(program);
                if crate::unix::is_executable_file(&candidate) {
                    return Ok(candidate.into_os_string());
                }
            }
        }
        Err(crate::Error::Invalid(format!(
            "unable to spawn {program:?}: not found on the filesystem and not found in PATH"
        )))
    }

    /// The resolved program path and full argv (including argv[0] as
    /// originally specified, not the resolved path — matching `execvp`'s
    /// convention of leaving `argv[0]` as the caller's chosen display name).
    pub(crate) fn exec_argv(&self) -> crate::Result<(std::ffi::CString, Vec<std::ffi::CString>)> {
        let resolved = self.resolve_program()?;
        let program = cstring_from_os(&resolved)?;
        let argv = self
            .args
            .iter()
            .map(|arg| cstring_from_os(arg))
            .collect::<crate::Result<Vec<_>>>()?;
        Ok((program, argv))
    }

    /// The full `KEY=VALUE` environment block as `CString`s for `execve`.
    pub(crate) fn exec_envp(&self) -> crate::Result<Vec<std::ffi::CString>> {
        self.envs
            .values()
            .map(|entry| {
                let mut bytes = entry.preferred_key.as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(entry.value.as_bytes());
                std::ffi::CString::new(bytes).map_err(|_| {
                    crate::Error::Invalid(
                        "environment variable contains an embedded NUL byte".into(),
                    )
                })
            })
            .collect()
    }

    /// The working directory to `chdir()` into before `execve`, if set.
    pub(crate) fn exec_cwd(&self) -> crate::Result<Option<std::ffi::CString>> {
        self.cwd.as_deref().map(cstring_from_os).transpose()
    }
}

#[cfg(unix)]
fn cstring_from_os(value: &OsStr) -> crate::Result<std::ffi::CString> {
    std::ffi::CString::new(value.as_bytes())
        .map_err(|_| crate::Error::Invalid(format!("{value:?} contains an embedded NUL byte")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_and_removal() {
        let mut cmd = CommandBuilder::new("dummy");
        cmd.env_clear();
        cmd.env("foo", "bar");
        assert_eq!(cmd.get_env("foo"), Some(OsStr::new("bar")));
        cmd.env_remove("foo");
        assert_eq!(cmd.get_env("foo"), None);
    }

    #[test]
    fn argv_and_cwd_roundtrip() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.args(["a", "b"]);
        cmd.cwd("/tmp");
        assert_eq!(
            cmd.get_argv(),
            &vec![
                OsString::from("prog"),
                OsString::from("a"),
                OsString::from("b")
            ]
        );
        assert_eq!(cmd.get_cwd(), Some(&OsString::from("/tmp")));
    }

    #[test]
    #[cfg(windows)]
    fn env_lookup_is_case_insensitive() {
        let mut cmd = CommandBuilder::new("dummy");
        cmd.env("Cargo_Pkg_Authors", "Nemo");
        assert_eq!(
            cmd.get_env("cargo_pkg_authors"),
            Some(OsStr::new("Nemo"))
        );
        cmd.env_remove("cARGO_pKG_aUTHORS");
        assert!(cmd.get_env("CARGO_PKG_AUTHORS").is_none());
    }

    #[test]
    #[cfg(windows)]
    fn append_quoted_handles_embedded_quotes_and_backslashes() {
        let mut out = Vec::new();
        CommandBuilder::append_quoted(OsStr::new(r#"C:\path with spaces\a.exe"#), &mut out);
        let quoted = String::from_utf16(&out).unwrap();
        assert_eq!(quoted, r#""C:\path with spaces\a.exe""#);

        let mut out = Vec::new();
        CommandBuilder::append_quoted(OsStr::new(r#"a"b"#), &mut out);
        let quoted = String::from_utf16(&out).unwrap();
        assert_eq!(quoted, r#""a\"b""#);

        let mut out = Vec::new();
        CommandBuilder::append_quoted(OsStr::new("noquotesneeded"), &mut out);
        let quoted = String::from_utf16(&out).unwrap();
        assert_eq!(quoted, "noquotesneeded");
    }

    #[test]
    #[cfg(unix)]
    fn resolve_program_accepts_explicit_path() {
        let cmd = CommandBuilder::new("/bin/sh");
        let (program, argv) = cmd.exec_argv().unwrap();
        assert_eq!(program.to_str().unwrap(), "/bin/sh");
        assert_eq!(argv[0].to_str().unwrap(), "/bin/sh");
    }

    #[test]
    #[cfg(unix)]
    fn resolve_program_searches_path() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.env("PATH", "/bin:/usr/bin");
        let (program, _argv) = cmd.exec_argv().unwrap();
        assert!(program.to_str().unwrap().ends_with("/sh"));
    }
}
