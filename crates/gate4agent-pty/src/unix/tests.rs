//! Unix (macOS/Linux) pty backend integration tests. Real `/bin/sh` child
//! processes, no mocking — these exercise the exact
//! `crate::native_pty_system()` public surface consumers use.

use std::io::Read;

use crate::PtySize;

fn open_pair() -> crate::PtyPair {
    crate::native_pty_system()
        .openpty(PtySize::default())
        .expect("openpty")
}

fn read_to_end_on_thread(
    mut reader: Box<dyn Read + Send>,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).map(|_| output)
    })
}

#[test]
fn spawn_sh_printf_marker_is_readable_from_master() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "printf gate4agent-pty-marker"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().expect("try_clone_reader");
    let reader_thread = read_to_end_on_thread(reader);

    let status = child.wait().expect("wait");
    drop(pair.master);
    let output = reader_thread
        .join()
        .expect("reader thread panicked")
        .expect("reader thread I/O error");

    assert!(status.success(), "/bin/sh exited with {status}");
    assert!(
        output
            .windows(b"gate4agent-pty-marker".len())
            .any(|window| window == b"gate4agent-pty-marker"),
        "marker not observed in pty output: {:?}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn resize_is_visible_to_a_child_via_stty_size() {
    let pair = open_pair();
    pair.master
        .resize(PtySize {
            rows: 31,
            cols: 97,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    let size = pair.master.get_size().expect("get_size");
    assert_eq!(size.rows, 31);
    assert_eq!(size.cols, 97);

    let mut cmd = crate::CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "stty size"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().expect("try_clone_reader");
    let reader_thread = read_to_end_on_thread(reader);
    let status = child.wait().expect("wait");
    drop(pair.master);
    let output = reader_thread
        .join()
        .expect("reader thread panicked")
        .expect("reader thread I/O error");
    let text = String::from_utf8_lossy(&output);

    assert!(status.success(), "stty size exited with {status}");
    assert!(
        text.contains("31 97"),
        "stty size did not report the resized window ({} rows x {} cols): {text:?}",
        size.rows,
        size.cols
    );
}

#[test]
fn kill_terminates_and_wait_reports_a_failed_exit() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "sleep 30"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    child.kill().expect("kill");
    let status = child.wait().expect("wait");
    assert!(!status.success(), "a killed process must not report success");
}

#[test]
fn wait_is_idempotent_and_a_second_call_does_not_hang_or_error() {
    // This exercises the same guarantee as "waitpid twice must not surface
    // as a hang or a surprising error": UnixChild caches the exit status
    // after the first successful wait(), so a second wait()/try_wait() call
    // never re-issues waitpid() against an already-reaped pid — it simply
    // returns the cached status, which is the correct behavior for a pid
    // the kernel may since have recycled for an unrelated process.
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "exit 7"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    let first = child.wait().expect("first wait");
    assert_eq!(first.exit_code(), 7);

    let second = child.wait().expect("second wait must not hang or error");
    assert_eq!(second.exit_code(), 7);

    let third = child.try_wait().expect("try_wait after wait");
    assert_eq!(third.map(|status| status.exit_code()), Some(7));
}

#[test]
fn fd_count_is_stable_across_two_spawns_best_effort() {
    // No let-else in this module: the Linux verification boxes run rustc 1.63.
    let before = match std::fs::read_dir("/dev/fd") {
        Ok(before_dir) => before_dir.count(),
        // /dev/fd is unavailable in this environment; fd-hygiene coverage
        // here is explicitly best-effort.
        Err(_) => return,
    };

    for _ in 0..2 {
        let pair = open_pair();
        let mut cmd = crate::CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
        drop(pair.slave);
        let status = child.wait().expect("wait");
        assert!(status.success());
        drop(pair.master);
    }

    let after = match std::fs::read_dir("/dev/fd") {
        Ok(after_dir) => after_dir.count(),
        Err(_) => return,
    };
    assert!(
        after <= before + 2,
        "fd count grew from {before} to {after} across two spawn/teardown cycles"
    );
}
