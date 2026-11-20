//! `chs-hook-emit` (§4.3): reads the hook payload from stdin and POSTs it to
//! the receiver. Always exits 0 and never writes stdout (Claude injects hook
//! stdout into context; exit 2 would block the prompt).

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::http_client::{self, parse_local_url};
use crate::{os, util};

pub const MAX_STDIN: usize = 4 * 1024 * 1024;
pub const CONNECT: Duration = Duration::from_secs(1);
pub const TOTAL: Duration = Duration::from_secs(3);

fn log(msg: &str) {
    let Some(path) = std::env::var_os("CHALDEA_HOOK_LOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            f,
            "{} chs-hook-emit pid={} {msg}",
            util::now_ms(),
            std::process::id()
        );
    }
}

pub fn main() -> ! {
    let deadline = Instant::now() + TOTAL;
    // One budget for the whole run (stdin, ancestry lookup, logging, POST):
    // the timer exits 0 whatever the main thread is blocked on.
    std::thread::spawn(move || {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        std::process::exit(0)
    });
    let agent = std::env::args().nth(1).unwrap_or_else(|| "unknown".into());
    // Distinguishes "never ran" from a quiet no-env exit (only with a log path).
    log(&format!("ran agent={agent}"));
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    // Missing or non-exact URL / missing token: exit at once without reading stdin.
    let Some(url) = env("CHALDEA_HOOK_URL").and_then(|u| parse_local_url(&u, "/hook")) else {
        std::process::exit(0)
    };
    let Some(token) = env("CHALDEA_HOOK_TOKEN") else {
        std::process::exit(0)
    };
    let thread = env("CHALDEA_THREAD_ID");
    if thread.is_none() {
        log("env_missing: CHALDEA_THREAD_ID");
    }

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = std::io::stdin()
            .lock()
            .take(MAX_STDIN as u64 + 1)
            .read_to_end(&mut buf)
            .map(|_| buf);
        let _ = tx.send(r);
    });
    let body = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(b)) if b.len() > MAX_STDIN => {
            log("stdin over 4 MiB; not sent");
            std::process::exit(0)
        }
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            log(&format!("stdin error: {e}"));
            std::process::exit(0)
        }
        Err(_) => {
            log("stdin still open past the 3 s budget; not sent");
            std::process::exit(0)
        }
    };

    let ancestry = util::encode_ancestry(&os::ancestry(std::process::id(), 8));
    let auth = format!("Bearer {token}");
    let agent_h = util::percent_encode(&agent);
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", &auth),
        ("Content-Type", "application/json"),
        ("X-Chaldea-Agent", &agent_h),
        ("X-Chaldea-Emitter-Version", util::EMITTER_VERSION),
        ("X-Chaldea-Emitter-Ancestry", &ancestry),
    ];
    if let Some(t) = thread.as_deref() {
        headers.push(("X-Chaldea-Thread-Id", t));
    }
    match http_client::request(&url, "POST", &headers, Some(&body), CONNECT, deadline) {
        Ok((200, _)) => {}
        Ok((code, _)) => log(&format!("receiver answered {code}")),
        Err(e) => log(&format!("post failed: {e}")),
    }
    std::process::exit(0)
}
