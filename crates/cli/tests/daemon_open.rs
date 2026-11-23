//! `chaldea open` (design §2.13): PR8-CL-24's binary case. The code `open --print-url` prints
//! was minted with the owner token and no `Origin` (the route refuses any `Origin` and any
//! other token kind), and pairs exactly once.

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;

use support::{Proc, WAIT, chaldea, run_cmd, run_to_exit, state};

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

#[test]
fn print_url_pairs_once() {
    let d = state();
    let mut daemon = Proc::spawn(&mut run_cmd(&d.path, None));
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
