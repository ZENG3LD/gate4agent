//! One message into one already-running grok session, through its running leader:
//! `initialize` -> `session/load` -> `session/prompt` -> close. Starts no `grok`, changes no
//! config, answers no permission modal (those stay with the client that owns the session).
//!
//! usage: g4a-grok-push --session <id> --cwd <dir> (--text <text> | --text-file <path> | -)
//!        [--leader-socket <path>]   (default: $GROK_HOME/leader.sock, else ~/.grok/leader.sock)

use std::io::Read;
use std::path::PathBuf;

use gate4agent_wake::grok_leader::{grok_home, leader_socket, push_into_session};

const USAGE: &str = "usage: g4a-grok-push --session <id> --cwd <dir> (--text <text> | --text-file <path> | -) [--leader-socket <path>]";

fn fail(code: i32, msg: &str) -> ! {
    eprintln!("g4a-grok-push: {msg}");
    std::process::exit(code)
}

fn main() {
    let mut session = None;
    let mut cwd = None;
    let mut text: Option<String> = None;
    let mut sock: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--session" => session = args.next(),
            "--cwd" => cwd = args.next(),
            "--text" => text = args.next(),
            "--text-file" => {
                let p = args.next().unwrap_or_else(|| fail(64, USAGE));
                text = Some(std::fs::read_to_string(&p).unwrap_or_else(|e| fail(66, &format!("text file: {}", e.kind()))));
            }
            "-" => {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s).unwrap_or_else(|e| fail(66, &format!("stdin: {}", e.kind())));
                text = Some(s);
            }
            "--leader-socket" => sock = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(64, &format!("unknown argument {other:?}; {USAGE}")),
        }
    }
    let (Some(session), Some(cwd), Some(text)) = (session, cwd, text) else { fail(64, USAGE) };
    let text = text.trim().to_string();
    if session.trim().is_empty() || text.is_empty() {
        fail(64, USAGE);
    }
    let sock = sock.unwrap_or_else(|| {
        let home = grok_home().unwrap_or_else(|| fail(69, "no grok home (set GROK_HOME)"));
        leader_socket(&home, None)
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap_or_else(|_| fail(70, "runtime"));
    match rt.block_on(push_into_session(&sock, &session, &cwd, &text)) {
        Ok(()) => println!("delivered: session {session} answered the prompt"),
        Err(e) => fail(1, &format!("not delivered: {e}")),
    }
}
