#[cfg(windows)]
fn suppress_fault_dialogs() {
    unsafe {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        SetErrorMode(
            SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
        );
        let _ = WerSetFlags(WER_FAULT_REPORTING_NO_UI);
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};

    suppress_fault_dialogs();
    if std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return Err("run this canary through windows-headless-supervisor".into());
    }

    let pair = native_pty_system().openpty(PtySize {
        rows: 12,
        cols: 48,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new("cmd.exe");
    command.args([
        "/D",
        "/Q",
        "/V:ON",
        "/C",
        "set /p g4a=& echo gate4agent-conpty:!g4a!",
    ]);
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let reader_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).map(|_| output)
    });
    let mut writer = pair.master.take_writer()?;
    pair.master.resize(PtySize {
        rows: 20,
        cols: 72,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    writer.write_all(b"ok\r\n")?;
    writer.flush()?;

    let status = child.wait()?;
    drop(writer);
    drop(pair.master);
    let output = reader_thread
        .join()
        .map_err(|_| std::io::Error::other("ConPTY reader thread panicked"))??;
    if !status.success() {
        return Err(format!("ConPTY fixture exited with {}", status.exit_code()).into());
    }
    if !output
        .windows(b"gate4agent-conpty:ok".len())
        .any(|window| window == b"gate4agent-conpty:ok")
    {
        return Err("ConPTY fixture sentinel was not observed".into());
    }

    println!(
        "gate4agent-conpty-canary-ok pointer_width={}",
        std::mem::size_of::<usize>() * 8
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("windows-conpty-canary-skipped");
}
