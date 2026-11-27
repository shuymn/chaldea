//! `chaldea open` (design §2.13): PR8-CL-24's binary case. The code `open --print-url` prints
//! was minted with the owner token and no `Origin` (the route refuses any `Origin` and any
//! other token kind), and pairs exactly once. `open_without_pwa` (docs/design/w1-pwa.md §2.1,
//! W1-9): the interactive form against a daemon without the PWA, run as this binary with test
//! seams (`support::faults`), whose launcher records instead of opening a browser.

mod support;

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::TcpStream;

use support::faults::{LAUNCHER, Labels, NO_PWA, cmd};
use support::runner::{Case, main as run_cases};
use support::{
    Proc, WAIT, chaldea, get_status, request_complete, reserve_port, run_cmd, run_to_exit, state,
};

fn main() {
    run_cases(&[
        Case {
            name: "print_url_pairs_once",
            run: print_url_pairs_once,
        },
        Case {
            name: "open_without_pwa",
            run: open_without_pwa,
        },
    ]);
}

/// `POST /api/pair` with a matching `Origin` and `{"code": …}`: the status.
fn pair(port: u16, code: &str) -> u16 {
    let body = format!(r#"{{"code":"{code}"}}"#);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    write!(
        s,
        "POST /api/pair HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut head = [0u8; 12];
    s.read_exact(&mut head).unwrap();
    std::str::from_utf8(&head[9..12]).unwrap().parse().unwrap()
}

fn print_url_pairs_once() {
    let d = state();
    let mut daemon = Proc::spawn(&mut run_cmd(&d.path, Some(reserve_port())));
    let port = daemon.serving();
    let (code, out, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(&d.path)
            .args(["open", "--print-url"]),
    );
    assert_eq!(code, 0, "{out}{err}");
    let url = out.trim();
    let prefix = format!("http://127.0.0.1:{port}/#pair=");
    let pair_code = url.strip_prefix(&prefix).unwrap_or_else(|| panic!("{url}"));
    assert!(pair_code.starts_with("chdp_"), "{url}");
    assert!(!err.contains(pair_code), "the code reached stderr: {err}");
    assert_eq!(pair(port, pair_code), 200);
    assert_eq!(pair(port, pair_code), 401);
    assert_eq!(daemon.stop_and_wait(&d.path), 0);
    let (code, _, err) = run_to_exit(
        chaldea(&[], &[])
            .arg("--state-dir")
            .arg(&d.path)
            .args(["open", "--print-url"]),
    );
    assert_eq!(code, 1);
    assert!(err.contains("daemon not running"), "{err}");
}

/// Interactive `open` against a daemon whose `GET /` is 501: exit 1 with the message, no
/// launch, and no code minted (all 8 pair-code slots stay free).
fn open_without_pwa() {
    let labels = Labels::new();
    let d = state();
    let record = tempfile::tempdir().unwrap();
    let launched = record.path().join("launched");
    let mut daemon = Proc::spawn(&mut cmd(
        &labels,
        &d.path,
        &["daemon", "run"],
        &[(NO_PWA, OsStr::new("1"))],
    ));
    let port = daemon.serving();
    assert_eq!(get_status(port, "/", None), 501);
    let (code, out, err) = run_to_exit(&mut cmd(
        &labels,
        &d.path,
        &["open"],
        &[(LAUNCHER, launched.as_os_str())],
    ));
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        err.contains("this chaldea was built without the PWA; build it with the release command"),
        "{err}"
    );
    assert!(out.is_empty(), "{out}");
    assert!(!launched.exists(), "the launcher ran");
    let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
    for n in 0..8 {
        let (status, body) = request_complete(port, "POST", "/api/pair-codes", &token, Some(""));
        assert_eq!(status, 200, "code {n}: {body}");
    }
    let (status, body) = request_complete(port, "POST", "/api/pair-codes", &token, Some(""));
    assert_eq!(status, 503, "a ninth code: {body}");
    assert_eq!(daemon.stop_and_wait(&d.path), 0);
}
