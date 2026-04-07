//! Smoke test: spawn Claude in PTY mode, read raw output for 15 seconds.

use gate4agent::pty::PtySession;
use gate4agent::{AgentEvent, SessionConfig};

#[tokio::main]
async fn main() {
    println!("=== gate4agent PTY mirror smoke test ===");
    println!("Spawning Claude in PTY mode...");

    let config = SessionConfig::default();

    let session = PtySession::spawn(config).await.expect("Failed to spawn PTY");
    println!("PTY spawned, session_id={}", session.session_id());

    let mut rx = session.subscribe();

    // Read events for 15 seconds
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut event_count = 0;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                println!("\n[15s timeout reached]");
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        event_count += 1;
                        match &event {
                            AgentEvent::PtyRaw { data } => {
                                // Print raw data (may contain ANSI sequences)
                                print!("{}", String::from_utf8_lossy(data));
                            }
                            AgentEvent::PtyReady => {
                                println!("\n[PTY READY - prompt detected]");
                            }
                            AgentEvent::PtyParsed(_msg) => {
                                // Just count, don't spam
                            }
                            AgentEvent::Started { session_id } => {
                                println!("[STARTED] {}", session_id);
                            }
                            AgentEvent::Exited { code } => {
                                println!("[EXITED] code={}", code);
                                break;
                            }
                            AgentEvent::Error { message } => {
                                println!("[ERROR] {}", message);
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        println!("[RX ERROR] {:?}", e);
                        break;
                    }
                }
            }
        }
    }

    println!("\nTotal events received: {}", event_count);
    let _ = session.kill().await;
    println!("=== Done ===");
}
