//! One attempt of the emitter (hook-ingest §2.13 step 4): connect, write the head and the
//! body, read the status line, each socket call bounded by the attempt's absolute deadline.
//!
//! Before every socket call the remaining time is computed; when none is left, the attempt
//! ends as a timeout without the call (`set_read_timeout` and `set_write_timeout` reject a
//! zero duration). A status line first observed at or after the deadline is a timeout too.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::Clock;

/// The connect bound of an attempt (A2: a refused loopback connect can take about 2 s on
/// Windows).
pub(crate) const CONNECT: Duration = Duration::from_millis(500);
/// The status line is read up to its first CRLF, within this many bytes.
pub(crate) const STATUS_LINE_MAX: usize = 1024;
/// The body is written in pieces of at most this size, each with its own write timeout.
const WRITE_CHUNK: usize = 64 * 1024;

/// How an attempt ended without a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// No time left before a socket call, or a status line observed after the deadline.
    Timeout,
    /// A connect, write, or read error (a reset, a timeout reported by the OS).
    Io(io::ErrorKind),
    /// EOF before a complete status line.
    Eof,
    /// A status line that is not `HTTP/1.<d> <ddd>` followed by SP or CRLF, or none within
    /// [`STATUS_LINE_MAX`] bytes.
    Malformed,
}

/// The time left until `deadline`, or `None` when it is reached.
pub(crate) fn remaining(deadline: Instant, now: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(now);
    (!left.is_zero()).then_some(left)
}

/// The socket calls an attempt makes.
pub(crate) trait Wire {
    type Conn: Conn;
    fn connect(&mut self, timeout: Duration) -> io::Result<Self::Conn>;
}

pub(crate) trait Conn {
    fn set_write_timeout(&mut self, d: Duration) -> io::Result<()>;
    fn write(&mut self, b: &[u8]) -> io::Result<usize>;
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()>;
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize>;
}

/// Loopback TCP to `127.0.0.1:<port>`, whatever the `Host` header names.
pub(crate) struct Tcp {
    pub(crate) port: u16,
}

impl Wire for Tcp {
    type Conn = TcpStream;

    fn connect(&mut self, timeout: Duration) -> io::Result<TcpStream> {
        let s =
            TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.port)), timeout)?;
        let _ = s.set_nodelay(true);
        Ok(s)
    }
}

impl Conn for TcpStream {
    fn set_write_timeout(&mut self, d: Duration) -> io::Result<()> {
        TcpStream::set_write_timeout(self, Some(d))
    }
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        Write::write(self, b)
    }
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        TcpStream::set_read_timeout(self, Some(d))
    }
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        Read::read(self, b)
    }
}

/// Runs one attempt: the status code of the reply, or how it ended without one.
pub(crate) fn attempt(
    clock: &dyn Clock,
    wire: &mut impl Wire,
    head: &[u8],
    body: &[u8],
    deadline: Instant,
) -> Result<u16, Transport> {
    let left = remaining(deadline, clock.now()).ok_or(Transport::Timeout)?;
    let mut c = wire.connect(left.min(CONNECT)).map_err(io_err)?;
    write_all(clock, &mut c, head, deadline)?;
    write_all(clock, &mut c, body, deadline)?;
    read_status(clock, &mut c, deadline)
}

#[allow(clippy::needless_pass_by_value, reason = "a `map_err` adapter")]
fn io_err(e: io::Error) -> Transport {
    Transport::Io(e.kind())
}

fn write_all(
    clock: &dyn Clock,
    c: &mut impl Conn,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), Transport> {
    while !bytes.is_empty() {
        let left = remaining(deadline, clock.now()).ok_or(Transport::Timeout)?;
        c.set_write_timeout(left).map_err(io_err)?;
        match c.write(&bytes[..bytes.len().min(WRITE_CHUNK)]) {
            Ok(0) => return Err(Transport::Io(io::ErrorKind::WriteZero)),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    Ok(())
}

fn read_status(clock: &dyn Clock, c: &mut impl Conn, deadline: Instant) -> Result<u16, Transport> {
    let mut buf = [0u8; STATUS_LINE_MAX];
    let mut len = 0;
    loop {
        let left = remaining(deadline, clock.now()).ok_or(Transport::Timeout)?;
        c.set_read_timeout(left).map_err(io_err)?;
        let n = match c.read(&mut buf[len..]) {
            Ok(0) => return Err(Transport::Eof),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(e)),
        };
        // The deadline wins over bytes observed at or after it.
        if remaining(deadline, clock.now()).is_none() {
            return Err(Transport::Timeout);
        }
        let from = len.saturating_sub(1);
        len += n;
        if let Some(i) = buf[from..len].windows(2).position(|w| w == b"\r\n") {
            return parse_status_line(&buf[..from + i + 2]).ok_or(Transport::Malformed);
        }
        if len == buf.len() {
            return Err(Transport::Malformed);
        }
    }
}

/// `HTTP/1.` + one digit + SP + three digits, followed by SP or CRLF; `line` ends at its
/// first CRLF.
pub(crate) fn parse_status_line(line: &[u8]) -> Option<u16> {
    let rest = line.strip_prefix(b"HTTP/1.")?;
    let [v, b' ', a, b, c, tail @ ..] = rest else {
        return None;
    };
    if !v.is_ascii_digit() || ![a, b, c].iter().all(|d| d.is_ascii_digit()) {
        return None;
    }
    if !(tail.first() == Some(&b' ') || tail == b"\r\n") {
        return None;
    }
    Some(u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0'))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn status_line_parse() {
        let ok: &[(&[u8], u16)] = &[
            (b"HTTP/1.1 200 OK\r\n", 200),
            (b"HTTP/1.0 503 Service Unavailable\r\n", 503),
            (b"HTTP/1.1 408\r\n", 408),
            (b"HTTP/1.1 401 \r\n", 401),
            (b"HTTP/1.9 999 x\r\n", 999),
        ];
        for (line, code) in ok {
            assert_eq!(parse_status_line(line), Some(*code), "{line:?}");
        }
        let bad: &[&[u8]] = &[
            b"",
            b"\r\n",
            b"HTTP/1.1 200",
            b"HTTP/1.1 200OK\r\n",
            b"HTTP/1.1 20 OK\r\n",
            b"HTTP/1.1 2000 OK\r\n",
            b"HTTP/1.1  200 OK\r\n",
            b"HTTP/2.0 200 OK\r\n",
            b"HTTP/1.x 200 OK\r\n",
            b"HTTP/1.1 2a0 OK\r\n",
            b"http/1.1 200 OK\r\n",
            b"HTTP/1.1\t200 OK\r\n",
            b"HTTP/1.11 200 OK\r\n",
            b" HTTP/1.1 200 OK\r\n",
        ];
        for line in bad {
            assert_eq!(parse_status_line(line), None, "{line:?}");
        }
    }

    /// A clock that moves only when told to, or by `step` at each reading.
    struct Manual {
        now: Cell<Instant>,
        step: Duration,
    }

    impl Clock for Manual {
        fn now(&self) -> Instant {
            let t = self.now.get();
            self.now.set(t + self.step);
            t
        }
        fn sleep(&self, d: Duration) {
            self.now.set(self.now.get() + d);
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Connect(Duration),
        WriteTimeout(Duration),
        Write(usize),
        ReadTimeout(Duration),
        Read,
    }

    #[derive(Default)]
    struct Script {
        calls: Vec<Call>,
        reads: VecDeque<io::Result<Vec<u8>>>,
        /// Time that passes inside each read (on the test clock).
        read_takes: Duration,
    }

    struct FakeWire {
        s: Rc<RefCell<Script>>,
        clock: Rc<Manual>,
        refuse: bool,
    }

    struct FakeConn {
        s: Rc<RefCell<Script>>,
        clock: Rc<Manual>,
    }

    impl Wire for FakeWire {
        type Conn = FakeConn;
        fn connect(&mut self, timeout: Duration) -> io::Result<FakeConn> {
            self.s.borrow_mut().calls.push(Call::Connect(timeout));
            if self.refuse {
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            Ok(FakeConn {
                s: self.s.clone(),
                clock: self.clock.clone(),
            })
        }
    }

    impl Conn for FakeConn {
        fn set_write_timeout(&mut self, d: Duration) -> io::Result<()> {
            assert!(!d.is_zero());
            self.s.borrow_mut().calls.push(Call::WriteTimeout(d));
            Ok(())
        }
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.s.borrow_mut().calls.push(Call::Write(b.len()));
            Ok(b.len())
        }
        fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
            assert!(!d.is_zero());
            self.s.borrow_mut().calls.push(Call::ReadTimeout(d));
            Ok(())
        }
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let mut s = self.s.borrow_mut();
            s.calls.push(Call::Read);
            self.clock.sleep(s.read_takes);
            match s.reads.pop_front().unwrap_or(Ok(Vec::new())) {
                Ok(bytes) => {
                    b[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Err(e) => Err(e),
            }
        }
    }

    fn rig(reads: Vec<io::Result<Vec<u8>>>) -> (Rc<Manual>, FakeWire, Rc<RefCell<Script>>) {
        let clock = Rc::new(Manual {
            now: Cell::new(Instant::now()),
            step: Duration::ZERO,
        });
        let s = Rc::new(RefCell::new(Script {
            reads: reads.into(),
            ..Script::default()
        }));
        let wire = FakeWire {
            s: s.clone(),
            clock: clock.clone(),
            refuse: false,
        };
        (clock, wire, s)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_reply_split_across_reads_is_parsed() {
        let (clock, mut wire, s) = rig(vec![
            Ok(b"HTTP/1.1 2".to_vec()),
            Ok(b"00 OK\r".to_vec()),
            Ok(b"\nContent-Length: 0\r\n".to_vec()),
        ]);
        let deadline = clock.now() + ms(1800);
        assert_eq!(
            attempt(&*clock, &mut wire, b"HEAD", b"BODY", deadline),
            Ok(200)
        );
        let calls = s.borrow().calls.clone();
        assert_eq!(calls[0], Call::Connect(CONNECT));
        assert_eq!(calls.iter().filter(|c| **c == Call::Read).count(), 3);
    }

    #[test]
    fn connect_is_bounded_by_the_remaining_time() {
        let (clock, mut wire, s) = rig(vec![Ok(b"HTTP/1.1 200\r\n".to_vec())]);
        let deadline = clock.now() + ms(120);
        assert_eq!(attempt(&*clock, &mut wire, b"H", b"", deadline), Ok(200));
        assert_eq!(s.borrow().calls[0], Call::Connect(ms(120)));
        // An empty body makes no write call.
        assert_eq!(
            s.borrow()
                .calls
                .iter()
                .filter(|c| matches!(c, Call::Write(_)))
                .count(),
            1
        );
    }

    #[test]
    fn zero_remaining_is_timeout() {
        // No time at the start: no connect.
        let (clock, mut wire, s) = rig(vec![]);
        let deadline = clock.now();
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"B", deadline),
            Err(Transport::Timeout)
        );
        assert!(s.borrow().calls.is_empty());

        // Time runs out after the connect: no write timeout is set with zero.
        let (clock, mut wire, s) = rig(vec![]);
        let deadline = clock.now() + ms(10);
        let stepping = Manual {
            now: Cell::new(clock.now()),
            step: ms(10),
        };
        assert_eq!(
            attempt(&stepping, &mut wire, b"H", b"B", deadline),
            Err(Transport::Timeout)
        );
        assert_eq!(s.borrow().calls, [Call::Connect(ms(10))]);
    }

    #[test]
    fn a_status_observed_at_the_deadline_is_a_timeout() {
        // The read returns the whole status line, but only once the deadline passed.
        let (clock, mut wire, s) = rig(vec![Ok(b"HTTP/1.1 200 OK\r\n".to_vec())]);
        s.borrow_mut().read_takes = ms(500);
        let deadline = clock.now() + ms(500);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"B", deadline),
            Err(Transport::Timeout)
        );
        // The same reply just before the deadline is the status.
        let (clock, mut wire, s) = rig(vec![Ok(b"HTTP/1.1 200 OK\r\n".to_vec())]);
        s.borrow_mut().read_takes = ms(499);
        let deadline = clock.now() + ms(500);
        assert_eq!(attempt(&*clock, &mut wire, b"H", b"B", deadline), Ok(200));
    }

    #[test]
    fn transport_endings() {
        let (clock, mut wire, _) = rig(vec![]);
        wire.refuse = true;
        let deadline = clock.now() + ms(100);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"", deadline),
            Err(Transport::Io(io::ErrorKind::ConnectionRefused))
        );
        let (clock, mut wire, _) = rig(vec![Ok(b"HTTP/1.1 20".to_vec())]);
        let deadline = clock.now() + ms(100);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"", deadline),
            Err(Transport::Eof)
        );
        let (clock, mut wire, _) = rig(vec![Err(io::ErrorKind::ConnectionReset.into())]);
        let deadline = clock.now() + ms(100);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"", deadline),
            Err(Transport::Io(io::ErrorKind::ConnectionReset))
        );
        let (clock, mut wire, _) = rig(vec![Ok(b"SSH-2.0\r\n".to_vec())]);
        let deadline = clock.now() + ms(100);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"", deadline),
            Err(Transport::Malformed)
        );
        // No CRLF within 1 KiB.
        let (clock, mut wire, _) = rig(vec![Ok(vec![b'x'; 600]), Ok(vec![b'y'; 424])]);
        let deadline = clock.now() + ms(100);
        assert_eq!(
            attempt(&*clock, &mut wire, b"H", b"", deadline),
            Err(Transport::Malformed)
        );
    }

    #[test]
    fn large_bodies_are_written_in_bounded_pieces() {
        let (clock, mut wire, s) = rig(vec![Ok(b"HTTP/1.1 200\r\n".to_vec())]);
        let body = vec![0u8; WRITE_CHUNK * 2 + 1];
        let deadline = clock.now() + ms(100);
        assert_eq!(attempt(&*clock, &mut wire, b"H", &body, deadline), Ok(200));
        let writes: Vec<usize> = s
            .borrow()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Write(n) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(writes, [1, WRITE_CHUNK, WRITE_CHUNK, 1]);
    }
}
