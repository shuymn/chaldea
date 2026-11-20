//! `chs-fake-cli` (§4.5): a deterministic stand-in for an agent CLI. Talks to
//! the harness only through the PTY and localhost HTTP (hooks via the real
//! emitter, `/witness` for liveness and FakeCli-side reports).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::http_client::{self, parse_local_url};
use crate::paste::{PASTE_END, PASTE_START};
use crate::{os, util};

/// Unknown-event payload sent verbatim by `echo` (B-11); odd spacing on purpose.
pub const UNKNOWN_PAYLOAD: &str = "{ \"hook_event_name\" : \"ChsFutureEvent\",\n  \"session_id\": null, \"x\": [1, 2.50, \"日本\"] }";

/// Lines `stall` draws; the harness's expected grid is authored separately in `scenario::fake`.
pub fn draw_stall() -> Vec<u8> {
    let mut s = String::new();
    s.push_str("\x1b[?1049h\x1b[?2004h\x1b[2J");
    s.push_str("\x1b[1;1HDo you trust the files in this folder?");
    s.push_str("\x1b[2;1H日本語の表示テスト｜ｶﾀｶﾅ");
    s.push_str("\x1b[3;1He\u{301} か\u{3099} 😀 end");
    s.push_str("\x1b[4;1Habc\x1b[4;999H漢字");
    s.push_str("\x1b[7;3H");
    s.into_bytes()
}

#[derive(Default, Clone)]
struct Opts {
    mode: String,
    emitter: Option<PathBuf>,
    session_id: Option<String>,
    role: String,
    nested: bool,
    nested_child: bool,
    ignore_input: bool,
    query_mid_paste: bool,
    silent: bool,
}

fn parse() -> Opts {
    let mut o = Opts {
        role: "plain".into(),
        ..Default::default()
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--mode" => {
                o.mode = next;
                i += 1;
            }
            "--emitter" => {
                o.emitter = Some(PathBuf::from(next));
                i += 1;
            }
            "--session-id" => {
                o.session_id = Some(next);
                i += 1;
            }
            "--role" => {
                o.role = next;
                i += 1;
            }
            "--nested" => o.nested = true,
            "--nested-child" => o.nested_child = true,
            "--ignore-input" => o.ignore_input = true,
            "--query-mid-paste" => o.query_mid_paste = true,
            "--silent" => o.silent = true,
            _ => {}
        }
        i += 1;
    }
    o
}

fn self_start_time() -> u64 {
    os::proc_info(std::process::id())
        .map(|p| p.start_time)
        .unwrap_or(0)
}

/// POST /witness; failures are ignored (a missing witness makes the harness inconclusive).
pub fn witness(role: &str, session: bool, report: Value) {
    let (Some(url), Some(token)) = (
        std::env::var("CHALDEA_HOOK_URL").ok(),
        std::env::var("CHALDEA_HOOK_TOKEN").ok(),
    ) else {
        return;
    };
    let Some(u) = parse_local_url(&url, "/hook") else {
        return;
    };
    let body = json!({
        "pid": std::process::id(),
        "start_time": self_start_time(),
        "role": role,
        "session": session,
        "report": report,
    });
    let auth = format!("Bearer {token}");
    let _ = http_client::request(
        &u.with_path("/witness"),
        "POST",
        &[("Authorization", &auth)],
        Some(body.to_string().as_bytes()),
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(3),
    );
}

fn out(bytes: &[u8]) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(bytes);
    let _ = o.flush();
}

fn emit(o: &Opts, payload: &[u8]) -> bool {
    let Some(em) = &o.emitter else { return false };
    let Ok(mut child) = Command::new(em)
        .arg("fake")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(payload);
    }
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

fn session_start(o: &Opts, sid: &str) -> bool {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let p = json!({
        "session_id": sid,
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "fake",
        "source": "startup",
    });
    emit(o, p.to_string().as_bytes())
}

/// Stdin bytes as chunks from a reader thread.
fn input_chunks() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let mut r = os::stdin_reader();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Finds a complete `ESC [ ... R` cursor-position report.
fn has_cpr(b: &[u8]) -> bool {
    let mut i = 0;
    while let Some(p) = find(&b[i..], b"\x1b[") {
        let s = i + p + 2;
        let tail = &b[s..];
        let n = tail
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == b';')
            .count();
        if n > 0 && tail.get(n) == Some(&b'R') {
            return true;
        }
        i = s;
    }
    false
}

fn run_echo(o: &Opts) -> i32 {
    let _raw = os::enable_raw_mode().ok();
    out(b"\x1b[?2004h");
    witness("echo", true, json!({}));
    let sid = o
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if o.nested {
        // B-11: a nested FakeCli inheriting the same env sends SessionStart before and after ours.
        let exe = std::env::current_exe().unwrap_or_default();
        let mut cmd = Command::new(exe);
        cmd.args(["--mode", "echo", "--nested-child"]);
        if let Some(e) = &o.emitter {
            cmd.arg("--emitter").arg(e);
        }
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let (tx, rx) = mpsc::channel();
            let stdout = child.stdout.take();
            std::thread::spawn(move || {
                if let Some(s) = stdout {
                    for line in BufReader::new(s).lines().map_while(Result::ok) {
                        let _ = tx.send(line);
                    }
                }
            });
            let _ = rx.recv_timeout(Duration::from_secs(5));
            session_start(o, &sid);
            emit(o, UNKNOWN_PAYLOAD.as_bytes());
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(b"go\n");
            }
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end && !matches!(child.try_wait(), Ok(Some(_))) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    } else {
        session_start(o, &sid);
    }
    if o.ignore_input {
        // B-10: stop reading so the harness writer blocks on a full PTY buffer.
        sleep_forever();
    }
    let rx = input_chunks();
    let mut buf: Vec<u8> = Vec::new();
    let mut paste: Option<Vec<u8>> = None;
    let mut reply_after = false;
    while let Ok(chunk) = rx.recv() {
        buf.extend_from_slice(&chunk);
        loop {
            if paste.is_none() {
                if let Some(p) = find(&buf, PASTE_START) {
                    buf.drain(..p + PASTE_START.len());
                    paste = Some(Vec::new());
                    reply_after = false;
                    if o.query_mid_paste {
                        out(b"\x1b[6n");
                    }
                    continue;
                }
                // Outside a paste: a CR submits the last completed paste.
                if let Some(p) = buf.iter().position(|b| *b == b'\r') {
                    let before: Vec<u8> = buf.drain(..=p).collect();
                    if has_cpr(&before) {
                        reply_after = true;
                    }
                    continue;
                }
                break;
            } else if let Some(p) = find(&buf, PASTE_END) {
                let mut body = paste.take().unwrap_or_default();
                body.extend(buf.drain(..p));
                buf.drain(..PASTE_END.len());
                // Wait for the CR that submits it (and any reply that lands before it).
                let mut tail: Vec<u8> = Vec::new();
                let end = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(p) = buf.iter().position(|b| *b == b'\r') {
                        tail.extend(buf.drain(..=p));
                        break;
                    }
                    tail.append(&mut buf);
                    match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                        Ok(c) => buf.extend_from_slice(&c),
                        Err(_) => break,
                    }
                }
                reply_after |= has_cpr(&tail);
                let (prompt, lossy) = match String::from_utf8(body.clone()) {
                    Ok(s) => (s, false),
                    Err(_) => (String::from_utf8_lossy(&body).into_owned(), true),
                };
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let p = json!({
                    "session_id": sid,
                    "transcript_path": null,
                    "cwd": cwd,
                    "hook_event_name": "UserPromptSubmit",
                    "prompt": prompt,
                    "chs_prompt_lossy": lossy,
                    "chs_wire_len": body.len(),
                    "chs_query_reply_after_paste": reply_after,
                });
                emit(o, p.to_string().as_bytes());
                continue;
            } else {
                // Keep a possible partial end marker in buf.
                let keep = PASTE_END.len() - 1;
                if buf.len() > keep {
                    let take = buf.len() - keep;
                    if let Some(pb) = paste.as_mut() {
                        pb.extend(buf.drain(..take));
                    }
                }
                break;
            }
        }
    }
    0
}

fn run_nested_child(o: &Opts) -> i32 {
    witness("echo-nested", true, json!({}));
    let sid = uuid::Uuid::new_v4().to_string();
    session_start(o, &sid);
    out(b"sent\n");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    session_start(o, &sid);
    0
}

struct Query {
    kind: &'static str,
    send: &'static [u8],
}

const QUERIES: &[Query] = &[
    Query {
        kind: "dsr6",
        send: b"\x1b[6n",
    },
    Query {
        kind: "dsr5",
        send: b"\x1b[5n",
    },
    Query {
        kind: "da1",
        send: b"\x1b[c",
    },
    Query {
        kind: "da2",
        send: b"\x1b[>c",
    },
    Query {
        kind: "osc10",
        send: b"\x1b]10;?\x1b\\",
    },
    Query {
        kind: "osc11",
        send: b"\x1b]11;?\x1b\\",
    },
    Query {
        kind: "kitty_keyboard",
        send: b"\x1b[?u",
    },
];

/// True when `b` holds the complete expected reply for `kind`.
pub fn reply_ok(kind: &str, b: &[u8]) -> bool {
    let s = String::from_utf8_lossy(b);
    match kind {
        "dsr6" => has_cpr(b),
        "dsr5" => s.contains("\x1b[0n"),
        "da1" => s.find("\x1b[?").is_some_and(|i| s[i..].contains('c')),
        "da2" => s.find("\x1b[>").is_some_and(|i| s[i..].contains('c')),
        "osc10" => s.contains("\x1b]10;rgb:d0d0/d0d0/d0d0"),
        "osc11" => s.contains("\x1b]11;rgb:1e1e/1e1e/1e1e"),
        "kitty_keyboard" => s.find("\x1b[?").is_some_and(|i| {
            let t = &s[i + 3..];
            let n = t.chars().take_while(char::is_ascii_digit).count();
            n > 0 && t[n..].starts_with('u')
        }),
        _ => false,
    }
}

fn run_queries() -> i32 {
    let _raw = os::enable_raw_mode().ok();
    out(b"\x1b[?2004h");
    witness("queries", true, json!({}));
    let rx = input_chunks();
    let mut results = Vec::new();
    let mut all = true;
    for q in QUERIES {
        while rx.try_recv().is_ok() {}
        let t = Instant::now();
        out(q.send);
        let mut got = Vec::new();
        let mut ok = false;
        let end = t + Duration::from_millis(500);
        while Instant::now() < end {
            match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(c) => {
                    got.extend_from_slice(&c);
                    if reply_ok(q.kind, &got) {
                        ok = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        all &= ok;
        results.push(json!({
            "kind": q.kind,
            "ok": ok,
            "latency_ms": ok.then(|| t.elapsed().as_secs_f64() * 1000.0),
            "received": String::from_utf8_lossy(&got).escape_debug().to_string(),
        }));
    }
    witness(
        "queries-result",
        true,
        json!({"all_ok": all, "results": results}),
    );
    if all { 0 } else { 1 }
}

fn run_stall(o: &Opts) -> i32 {
    let _raw = os::enable_raw_mode().ok();
    witness("stall", true, json!({}));
    if !o.silent {
        out(&draw_stall());
    }
    sleep_forever()
}

fn run_banner(o: &Opts) -> i32 {
    let _raw = os::enable_raw_mode().ok();
    out(b"\x1b[?2004h\x1b[2J\x1b[1;1HDo you trust the files in this folder?\x1b[2;1H> 1. Yes, proceed\x1b[3;1H  2. No, exit");
    witness("banner", true, json!({}));
    let sid = o
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    session_start(o, &sid);
    sleep_forever()
}

fn parse_keys(chunk: &[u8]) -> Vec<String> {
    let mut keys = Vec::new();
    match chunk {
        b"\x1b[B" | b"\x1bOB" => keys.push("down:vt".into()),
        b"\r" => keys.push("enter:vt".into()),
        b"\x1b" => keys.push("esc:vt".into()),
        _ => {
            // win32-input-mode: CSI Vk;Sc;Uc;Kd;Cs;Rc _
            let s = String::from_utf8_lossy(chunk);
            for seq in s.split("\x1b[").skip(1) {
                if let Some(body) = seq.strip_suffix('_').or_else(|| seq.split('_').next())
                    && let Some(vk) = body.split(';').next().and_then(|v| v.parse::<u32>().ok())
                {
                    let kd = body
                        .split(';')
                        .nth(3)
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let name = match vk {
                        0x28 => "down",
                        0x0d => "enter",
                        0x1b => "esc",
                        _ => continue,
                    };
                    if kd == 1 {
                        keys.push(format!("{name}:win32"));
                    }
                }
            }
        }
    }
    keys
}

fn run_attachable(o: &Opts) -> i32 {
    let _raw = os::enable_raw_mode().ok();
    out(b"\x1b[?2004h\x1b[2J");
    witness("attachable", true, json!({}));
    let sid = o
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    session_start(o, &sid);
    let emitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let em2 = emitted.clone();
    std::thread::spawn(move || {
        let mut n: u64 = 0;
        loop {
            n += 1;
            em2.lock().unwrap_or_else(|e| e.into_inner()).push(n);
            out(format!("\x1b[1;1HMARKER-{n:06}\x1b[K").as_bytes());
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    let rx = input_chunks();
    let mut nonces: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    while let Ok(chunk) = rx.recv() {
        if let Some(s) = find(&chunk, PASTE_START).or(if buf.is_empty() { None } else { Some(0) }) {
            buf.extend_from_slice(&chunk[s..]);
            if let Some(e) = find(&buf, PASTE_END) {
                let body = buf[PASTE_START.len().min(e)..e].to_vec();
                nonces.push(String::from_utf8_lossy(&body).into_owned());
                buf.clear();
            }
        } else {
            keys.extend(parse_keys(&chunk));
        }
        let markers = emitted.lock().unwrap_or_else(|e| e.into_inner()).clone();
        witness(
            "attachable-report",
            false,
            json!({"markers_emitted": markers, "nonces": nonces, "keys": keys}),
        );
    }
    0
}

fn run_spawn_tree(o: &Opts) -> i32 {
    let _raw = os::enable_raw_mode().ok();
    witness("spawn-tree", true, json!({}));
    let exe = std::env::current_exe().unwrap_or_default();
    let mut children = Vec::new();
    let mut first_child = None;
    for role in os::spawn_tree_roles() {
        let spawn_role = if *role == "double-forked" {
            "double-fork-parent"
        } else {
            role
        };
        let mut cmd = Command::new(&exe);
        cmd.args(["--mode", "sleeper", "--role", spawn_role]);
        if let Some(e) = &o.emitter {
            cmd.arg("--emitter").arg(e);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let res = if *role == "setsid" || *role == "breakaway" {
            os::spawn_escapee(&mut cmd).map(|(c, how)| (c, how.to_string()))
        } else {
            cmd.spawn().map(|c| (c, "plain".to_string()))
        };
        match res {
            Ok((c, how)) => {
                first_child.get_or_insert(c.id());
                children.push(json!({"role": role, "pid": c.id(), "how": how}));
                std::mem::forget(c);
            }
            Err(e) => children.push(json!({"role": role, "error": e.to_string()})),
        }
    }
    witness(
        "spawn-tree-report",
        true,
        json!({"first_child": first_child, "children": children}),
    );
    sleep_forever()
}

/// Minimal MCP stdio server: answers `initialize` and `tools/list`, then attests.
fn run_mcp() -> i32 {
    let stdin = std::io::stdin();
    let mut attested = false;
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let resp = match method {
            "initialize" => {
                let pv = msg
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or(json!("2025-06-18"));
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": pv,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "chs-sleeper", "version": util::EMITTER_VERSION},
                }})
            }
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            _ => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
            }
        };
        out(format!("{resp}\n").as_bytes());
        if method == "initialize" && !attested {
            attested = true;
            witness("mcp-sleeper", true, json!({"initialized": true}));
        }
    }
    0
}

fn run_sleeper(o: &Opts) -> i32 {
    match o.role.as_str() {
        "mcp" => return run_mcp(),
        "ignore-term" => os::ignore_term_signals(),
        "double-fork-parent" => {
            witness("double-fork-parent", true, json!({}));
            let exe = std::env::current_exe().unwrap_or_default();
            let _ = Command::new(exe)
                .args(["--mode", "sleeper", "--role", "double-forked"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(std::mem::forget);
            return 0;
        }
        _ => {}
    }
    witness(&o.role, o.role != "unrelated", json!({}));
    sleep_forever()
}

pub fn main() -> i32 {
    let o = parse();
    os::enable_vt_output();
    match o.mode.as_str() {
        "echo" if o.nested_child => run_nested_child(&o),
        "echo" => run_echo(&o),
        "queries" => run_queries(),
        "stall" => run_stall(&o),
        "banner" => run_banner(&o),
        "attachable" => run_attachable(&o),
        "spawn-tree" => run_spawn_tree(&o),
        "sleeper" => run_sleeper(&o),
        m => {
            eprintln!("chs-fake-cli: unknown --mode {m:?}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_matchers() {
        assert!(reply_ok("dsr6", b"\x1b[3;14R"));
        assert!(!reply_ok("dsr6", b"\x1b[3;14"));
        assert!(reply_ok("da1", b"\x1b[?6c"));
        assert!(reply_ok("da2", b"\x1b[>0;2600;1c"));
        assert!(reply_ok("osc11", b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"));
        assert!(reply_ok("kitty_keyboard", b"\x1b[?0u"));
        assert!(!reply_ok("kitty_keyboard", b"\x1b[?6c"));
    }

    #[test]
    fn win32_keys() {
        assert_eq!(
            parse_keys(b"\x1b[40;80;0;1;0;1_\x1b[40;80;0;0;0;1_"),
            vec!["down:win32"]
        );
        assert_eq!(parse_keys(b"\x1b[B"), vec!["down:vt"]);
    }
}
