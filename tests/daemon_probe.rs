//! Integration tests for `probe_daemon` — verifies the public API is
//! exported correctly and the happy-path probe works end-to-end.

use std::net::TcpListener;

use gate4agent::transport::DaemonProbe;
use gate4agent::daemon::probe_daemon;

/// Bind an OS-assigned port, probe it — should return Ok(()).
///
/// This is the integration-level sanity check that the public API is wired up
/// correctly. The unit-level tests in `src/daemon/probe.rs` cover error paths.
#[test]
fn probe_open_port_integration() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
    let port = listener.local_addr().expect("local_addr").port();

    let spec = DaemonProbe::new("127.0.0.1", port, 2000);
    let result = probe_daemon(&spec);

    drop(listener);

    assert!(
        result.is_ok(),
        "probe_daemon must return Ok(()) when a listener is bound on the target port: {:?}",
        result
    );
}
