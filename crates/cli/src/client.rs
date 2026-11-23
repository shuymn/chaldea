//! What a client of the daemon uses (docs/design/daemon-lifecycle.md §2.6, §2.13, §2.14):
//! discovery under the liveness rule, the owner token read with the daemon's own checks, and a
//! std-only HTTP/1.1 client that sends one request per connection, every read and write
//! bounded by the caller's deadline.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::time::Instant;

pub use chaldea_daemon::lifecycle::Discovery;

/// The largest response body read.
const MAX_BODY: usize = 1024 * 1024;
/// The largest response head read.
const MAX_HEAD: usize = 64 * 1024;

/// The liveness rule: a record only while the lock is held and its identity is alive.
#[must_use]
pub fn discover(dir: &Path) -> Discovery {
    chaldea_daemon::lifecycle::discover(dir)
}

/// The owner token, checked as the daemon checks it (private, a regular file, the token form).
///
/// # Errors
///
/// What is wrong with the file; the message never contains the token.
pub fn read_owner_token(dir: &Path) -> Result<String, String> {
    chaldea_daemon::lifecycle::read_owner_token(dir)
}

/// A response: the status code and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "the request did not finish in time",
    )
}

fn left(deadline: Instant) -> io::Result<std::time::Duration> {
    let d = deadline.saturating_duration_since(Instant::now());
    if d.is_zero() { Err(timed_out()) } else { Ok(d) }
}

/// One request on a new connection to `127.0.0.1:<port>` with `Connection: close`, no
/// `Origin`, and `Authorization: Bearer <token>` when given; a body is sent as JSON with its
/// `Content-Length` (0 when absent). The response must carry a `Content-Length` of at most
/// 1 MiB and be read completely before `deadline`.
///
/// # Errors
///
/// A connection, I/O, or protocol error; `TimedOut` at or past `deadline`, also for a response
/// whose last part was read at or after it.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    deadline: Instant,
) -> io::Result<Response> {
    let mut s = send(port, method, path, token, body, deadline)?;
    read_response(|buf| read_some(&mut s, buf, deadline), deadline)
}

/// The status of one bodiless request sent as [`request`] sends it: only the response head is
/// read (at most 64 KiB, before `deadline`), never the body, whatever its length.
///
/// # Errors
///
/// As [`request`].
pub fn request_status(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    deadline: Instant,
) -> io::Result<u16> {
    let mut s = send(port, method, path, token, None, deadline)?;
    let mut buf = Vec::new();
    let (status, _) = read_head(
        &mut checked(|buf| read_some(&mut s, buf, deadline), deadline),
        &mut buf,
    )?;
    Ok(status)
}

/// Connects and writes one request, before `deadline`.
fn send(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    deadline: Instant,
) -> io::Result<TcpStream> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, left(deadline)?)?;
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let content = body.map_or_else(
        || "Content-Length: 0\r\n".to_owned(),
        |b| {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            )
        },
    );
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}{content}Connection: close\r\n\r\n"
    )
    .into_bytes();
    if let Some(b) = body {
        req.extend_from_slice(b);
    }
    s.set_write_timeout(Some(left(deadline)?))?;
    s.write_all(&req)?;
    Ok(s)
}

/// `read` followed by the deadline check, before its own result: whatever a read returned at
/// or after `deadline` (data, an end, an error) is a timeout.
fn checked(
    mut read: impl FnMut(&mut Vec<u8>) -> io::Result<()>,
    deadline: Instant,
) -> impl FnMut(&mut Vec<u8>) -> io::Result<()> {
    move |buf| {
        let r = read(buf);
        left(deadline)?;
        r
    }
}

/// Reads into `buf` until the head ends: the status code and the head's length.
fn read_head(
    read: &mut impl FnMut(&mut Vec<u8>) -> io::Result<()>,
    buf: &mut Vec<u8>,
) -> io::Result<(u16, usize)> {
    let head_end = loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(protocol("the response head is too large"));
        }
        read(buf)?;
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| protocol("a non-UTF-8 head"))?;
    let status = head
        .split("\r\n")
        .next()
        .and_then(|l| l.strip_prefix("HTTP/1.1 "))
        .and_then(|l| l.get(..3))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| protocol("not an HTTP/1.1 status line"))?;
    Ok((status, head_end))
}

/// The response from successive `read`s into one buffer: complete only when every part was
/// read before `deadline` ([`checked`]).
fn read_response(
    read: impl FnMut(&mut Vec<u8>) -> io::Result<()>,
    deadline: Instant,
) -> io::Result<Response> {
    let mut buf = Vec::new();
    let mut read = checked(read, deadline);
    let (status, head_end) = read_head(&mut read, &mut buf)?;
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| protocol("a non-UTF-8 head"))?;
    let length = head
        .split("\r\n")
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
        })
        .flatten()
        .ok_or_else(|| protocol("no Content-Length"))?;
    if length > MAX_BODY {
        return Err(protocol("the response body is too large"));
    }
    let start = head_end + 4;
    while buf.len() < start + length {
        read(&mut buf)?;
    }
    Ok(Response {
        status,
        body: buf[start..start + length].to_vec(),
    })
}

fn protocol(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Reads what arrives within the deadline; EOF before the response is complete is an error.
fn read_some(s: &mut TcpStream, buf: &mut Vec<u8>, deadline: Instant) -> io::Result<()> {
    s.set_read_timeout(Some(left(deadline)?))?;
    let mut chunk = [0u8; 8192];
    match s.read(&mut chunk) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the connection closed before the response was complete",
        )),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(timed_out())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use chaldea_pty_runtime::host::{self, StopSent, StopTarget};

    use chaldea_daemon::lifecycle::spawn_locked;

    fn instance() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn soon() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(5)
    }

    /// The real identity lookup, `deliver` as the delivery.
    fn deliver<'a>(deliver: &'a dyn Fn(&StopTarget) -> std::io::Result<()>) -> host::StopSeams<'a> {
        host::StopSeams {
            identity: &host::process_identity,
            before_delivery: &|| {},
            deliver: Some(deliver),
        }
    }

    /// A live process this test owns, unrelated to any daemon.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        fn start() -> Sleeper {
            #[cfg(unix)]
            let mut c = Command::new("/bin/sleep");
            #[cfg(unix)]
            c.arg("60");
            #[cfg(windows)]
            let mut c = Command::new(format!(
                "{}\\System32\\ping.exe",
                std::env::var("SystemRoot").unwrap()
            ));
            #[cfg(windows)]
            c.args(["-n", "60", "127.0.0.1"]);
            c.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            Sleeper(spawn_locked(&mut c).unwrap())
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn alive(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// PR8-CL-15: a live pid with another start time is `IdentityMismatch`, and nothing is
    /// signalled.
    #[test]
    fn send_stop_mismatch() {
        let mut s = Sleeper::start();
        let t = StopTarget {
            pid: s.pid(),
            start_time: "1".to_owned(),
            instance: instance(),
        };
        assert_eq!(
            host::send_stop(&t, soon()).unwrap(),
            StopSent::IdentityMismatch
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(s.alive(), "the unrelated process was signalled");
    }

    /// PR8-CL-15: a target that exited before the check is `Exited`.
    #[test]
    fn send_stop_exited() {
        let mut s = Sleeper::start();
        let id = host::process_identity(s.pid()).unwrap().unwrap();
        let _ = s.0.kill();
        let _ = s.0.wait();
        let t = StopTarget {
            pid: id.pid,
            start_time: id.start_time,
            instance: instance(),
        };
        assert_eq!(host::send_stop(&t, soon()).unwrap(), StopSent::Exited);
    }

    /// PR8-CL-15 (Unix): `ESRCH` from `kill` after a matching check is `Exited`, not an error.
    #[cfg(unix)]
    #[test]
    fn send_stop_esrch_is_exited() {
        let mut s = Sleeper::start();
        let id = host::process_identity(s.pid()).unwrap().unwrap();
        let t = StopTarget {
            pid: id.pid,
            start_time: id.start_time,
            instance: instance(),
        };
        let esrch = |_: &StopTarget| -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(
                rustix::io::Errno::SRCH.raw_os_error(),
            ))
        };
        assert_eq!(
            host::send_stop_with(&t, soon(), &deliver(&esrch)).unwrap(),
            StopSent::Exited
        );
        assert!(s.alive());
    }

    /// PR8-CL-15: a matching target whose delivery is refused (`EPERM`, Windows
    /// `ERROR_ACCESS_DENIED`) is `AccessDenied`.
    #[test]
    fn send_stop_access_denied() {
        let mut s = Sleeper::start();
        let id = host::process_identity(s.pid()).unwrap().unwrap();
        let t = StopTarget {
            pid: id.pid,
            start_time: id.start_time,
            instance: instance(),
        };
        #[cfg(unix)]
        let denied = rustix::io::Errno::PERM.raw_os_error();
        #[cfg(windows)]
        let denied = 5;
        let refuse = |_: &StopTarget| -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(denied))
        };
        assert_eq!(
            host::send_stop_with(&t, soon(), &deliver(&refuse)).unwrap(),
            StopSent::AccessDenied
        );
        assert!(s.alive());
    }

    /// PR8-CL-24: a complete response whose last part is first read at or after the deadline
    /// is a timeout, not a response; the same response read before it is accepted.
    #[test]
    fn response_read_at_the_deadline_is_a_timeout() {
        use std::time::{Duration, Instant};
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let end = Instant::now() + Duration::from_millis(100);
        let late = super::read_response(
            |buf| {
                std::thread::sleep(end.saturating_duration_since(Instant::now()));
                buf.extend_from_slice(RESPONSE);
                Ok(())
            },
            end,
        );
        assert_eq!(
            late.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut,
            "a late response was accepted"
        );
        let on_time = super::read_response(
            |buf| {
                buf.extend_from_slice(RESPONSE);
                Ok(())
            },
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!((on_time.status, on_time.body), (200, b"{}".to_vec()));
    }

    /// A read that fails at or after the deadline (here an early end of the connection) is a
    /// timeout, not its own error.
    #[test]
    fn late_read_error_is_a_timeout() {
        use std::time::{Duration, Instant};
        let end = Instant::now() + Duration::from_millis(100);
        let late = super::read_response(
            |_| {
                std::thread::sleep(end.saturating_duration_since(Instant::now()));
                Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
            },
            end,
        );
        assert_eq!(late.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    /// PR8-CL-15 (Unix): an identity check that ends at or after the deadline sends nothing:
    /// the deadline is checked after the check, just before `kill`. With time left, the same
    /// check is followed by the signal.
    #[cfg(unix)]
    #[test]
    fn identity_check_past_the_deadline_signals_nothing() {
        use std::time::{Duration, Instant};
        let mut s = Sleeper::start();
        let id = host::process_identity(s.pid()).unwrap().unwrap();
        let t = StopTarget {
            pid: id.pid,
            start_time: id.start_time,
            instance: instance(),
        };
        let end = Instant::now() + Duration::from_millis(200);
        let slow_identity = |pid: u32| {
            let r = host::process_identity(pid);
            std::thread::sleep(end.saturating_duration_since(Instant::now()));
            r
        };
        let seams = host::StopSeams {
            identity: &slow_identity,
            before_delivery: &|| {},
            deliver: None,
        };
        assert_eq!(
            host::send_stop_with(&t, end, &seams).unwrap(),
            StopSent::DeadlinePassed
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(s.alive(), "signalled after the deadline");
        assert_eq!(host::send_stop(&t, soon()).unwrap(), StopSent::Sent);
    }

    /// PR8-CL-15 (Windows): a pause between `OpenEventW` and `SetEvent` that crosses the
    /// deadline sets nothing; with time left, the event is set.
    #[cfg(windows)]
    #[test]
    fn opened_event_past_the_deadline_is_not_set() {
        use std::time::{Duration, Instant};
        let me = host::current_identity().unwrap();
        let inst = instance();
        let (tx, rx) = std::sync::mpsc::channel();
        let _l = host::listen_for_stop(&inst, Box::new(move || tx.send(()).unwrap())).unwrap();
        let t = StopTarget {
            pid: me.pid,
            start_time: me.start_time,
            instance: inst,
        };
        let end = Instant::now() + Duration::from_millis(200);
        let pause = || std::thread::sleep(end.saturating_duration_since(Instant::now()));
        let seams = host::StopSeams {
            identity: &host::process_identity,
            before_delivery: &pause,
            deliver: None,
        };
        assert_eq!(
            host::send_stop_with(&t, end, &seams).unwrap(),
            StopSent::DeadlinePassed
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the event was set after the deadline"
        );
        assert_eq!(host::send_stop(&t, soon()).unwrap(), StopSent::Sent);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }

    /// The request client against a listener that answers once: status and body.
    #[test]
    fn request_reads_status_and_body() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 1024];
            while super::find(&got, b"\r\n\r\n").is_none() {
                let n = std::io::Read::read(&mut c, &mut b).unwrap();
                got.extend_from_slice(&b[..n]);
            }
            std::io::Write::write_all(
                &mut c,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .unwrap();
            String::from_utf8(got).unwrap()
        });
        let r = super::request(
            port,
            "POST",
            "/x",
            Some("t"),
            None,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{}");
        let req = server.join().unwrap();
        assert!(req.contains("Authorization: Bearer t\r\n"), "{req}");
        assert!(req.contains("Content-Length: 0\r\n"), "{req}");
        assert!(!req.to_ascii_lowercase().contains("origin:"), "{req}");
    }
}
