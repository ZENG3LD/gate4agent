//! Windows ConPTY backend integration tests. Real `cmd.exe` child processes,
//! no mocking — these exercise the exact `crate::native_pty_system()` public
//! surface consumers use.

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
fn spawn_cmd_echo_marker_is_readable_from_master() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("cmd.exe");
    cmd.args(["/D", "/Q", "/C", "echo gate4agent-pty-marker"]);
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

    assert!(status.success(), "cmd.exe exited with {status}");
    assert!(
        output
            .windows(b"gate4agent-pty-marker".len())
            .any(|window| window == b"gate4agent-pty-marker"),
        "marker not observed in ConPTY output: {:?}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn resize_succeeds_and_is_reflected_by_get_size() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("cmd.exe");
    cmd.args(["/D", "/Q", "/C", "pause >nul"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

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

    child.kill().expect("kill");
    child.wait().expect("wait");
}

#[test]
fn kill_terminates_and_wait_reports_a_failed_exit() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("cmd.exe");
    cmd.args(["/D", "/Q", "/C", "pause >nul"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    child.kill().expect("kill");
    let status = child.wait().expect("wait");
    assert!(!status.success(), "a killed process must not report success");
}

#[test]
fn process_id_is_a_real_positive_pid() {
    let pair = open_pair();
    let mut cmd = crate::CommandBuilder::new("cmd.exe");
    cmd.args(["/D", "/Q", "/C", "pause >nul"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
    drop(pair.slave);

    let pid = child
        .process_id()
        .expect("a live ConPTY child must report a process id");
    assert!(pid > 0, "expected a positive Windows process id, got {pid}");

    child.kill().expect("kill");
    child.wait().expect("wait");
}

#[test]
fn second_openpty_in_the_same_process_also_works() {
    // Regression coverage for the deliberate choice not to cache the
    // dynamically loaded ConPTY entry points in a process-lifetime static:
    // a second openpty() must re-resolve and succeed exactly like the
    // first.
    for _ in 0..2 {
        let pair = open_pair();
        let mut cmd = crate::CommandBuilder::new("cmd.exe");
        cmd.args(["/D", "/Q", "/C", "exit /b 0"]);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn_command");
        drop(pair.slave);
        let status = child.wait().expect("wait");
        assert!(status.success());
        drop(pair.master);
    }
}

#[test]
fn conpty_unavailable_error_type_constructs_and_displays() {
    let error = crate::Error::ConPtyUnavailable("no CreatePseudoConsole export".to_owned());
    assert!(matches!(error, crate::Error::ConPtyUnavailable(_)));
    assert!(error.to_string().contains("no CreatePseudoConsole export"));

    let call_error = crate::Error::ConPtyCall {
        operation: "CreatePseudoConsole",
        hresult: -1,
    };
    assert!(call_error.to_string().contains("CreatePseudoConsole"));
}
