//! Standalone cursor probe — no mylittlechart involved.
//!
//! Spawns `claude` in a PTY, feeds raw bytes into a local `vt100::Parser`
//! (same as gate4agent's manager does), and every 250ms prints:
//!   - vt100 cursor_position() -> (row, col)
//!   - hide_cursor() flag
//!   - the full row at `cursor_row`, with `|` markers at col-1 / col / col+1
//!
//! After ~4s it writes "hello" to the PTY and dumps cursor state again.
//! Run: `cargo run --example pty_cursor_probe -p gate4agent`

use gate4agent::pty::PtySession;
use gate4agent::{AgentEvent, SessionConfig};
use std::time::Duration;

const ROWS: u16 = 30;
const COLS: u16 = 100;

fn dump(parser: &vt100::Parser, label: &str) {
    let screen = parser.screen();
    let (r, c) = screen.cursor_position();
    let hidden = screen.hide_cursor();
    println!(
        "--- {label} --- cursor=({r},{c}) hide_cursor={hidden} rows={ROWS} cols={COLS}"
    );

    // Print the cursor row in full, with cell contents visualized.
    let mut line = String::new();
    for col in 0..COLS {
        if let Some(cell) = screen.cell(r, col) {
            let ch = cell.contents();
            if ch.is_empty() {
                line.push('.');
            } else {
                // Take first char (multi-codepoint cells are rare for CLIs).
                line.push(ch.chars().next().unwrap_or('?'));
            }
        } else {
            line.push('?');
        }
    }
    println!("row{r:02} |{line}|");

    // Carets line pointing at col-1 / col / col+1.
    let mut caret = String::new();
    for col in 0..COLS {
        if col + 1 == c {
            caret.push('<');
        } else if col == c {
            caret.push('^');
        } else if col == c + 1 {
            caret.push('>');
        } else {
            caret.push(' ');
        }
    }
    println!("      |{caret}|");

    // Also show the 5-cell neighborhood as codepoints.
    let lo = c.saturating_sub(2);
    let hi = (c + 2).min(COLS.saturating_sub(1));
    print!("      neighborhood:");
    for col in lo..=hi {
        if let Some(cell) = screen.cell(r, col) {
            let ch = cell.contents();
            let marker = if col == c { "*" } else { " " };
            print!(" {marker}[{col:02}]={:?}{marker}", ch);
        }
    }
    println!();
}

#[tokio::main]
async fn main() {
    println!("=== gate4agent cursor probe ===");

    let config = SessionConfig::default();

    let session = PtySession::spawn_with_size(config, ROWS, COLS)
        .await
        .expect("Failed to spawn PTY");
    println!("PTY spawned, session_id={}", session.session_id());

    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let mut rx = session.subscribe();

    let start = tokio::time::Instant::now();
    let total = Duration::from_secs(10);
    let mut next_dump = start + Duration::from_millis(500);
    let mut sent_hello = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= start + total {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep_until(next_dump) => {
                dump(&parser, "tick");
                next_dump = now + Duration::from_millis(500);

                // After 4s, type "hello" into the PTY.
                if !sent_hello && now >= start + Duration::from_secs(4) {
                    sent_hello = true;
                    println!(">>> writing \"hello\" to PTY");
                    let _ = session.write("hello").await;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(AgentEvent::PtyRaw { data }) => {
                        parser.process(&data);
                    }
                    Ok(AgentEvent::Exited { code }) => {
                        println!("[EXITED] code={code}");
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }

    dump(&parser, "final");
    let _ = session.kill().await;
    println!("=== Done ===");
}
