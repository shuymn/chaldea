//! The PTY ends (`docs/design/pty-unix.md` §2.3, §2.6): a nonblocking master read and
//! written through `poll`, both stopped by a level-triggered wake (a flag plus a pipe).

use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;
#[cfg(any(test, feature = "fake"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::termios::{Winsize, tcsetwinsize};

use crate::backend::{CloseReport, PtyControl, PtySize};

/// Bytes the reader still returns after the wake.
pub(crate) const AFTER_WAKE_CAP: u64 = 1 << 20;
/// Retries of the wake byte's write on `EINTR`.
const WAKE_RETRIES: usize = 3;

/// Entries of `MasterWriter` into its full-queue `poll` (test gate).
#[cfg(any(test, feature = "fake"))]
pub(crate) static WRITER_POLLS: AtomicU64 = AtomicU64::new(0);

/// The size with the 8×16 px cells the responder reports.
pub(crate) fn winsize(size: PtySize) -> Winsize {
    Winsize {
        ws_row: size.rows(),
        ws_col: size.cols(),
        ws_xpixel: size.cols() * 8,
        ws_ypixel: size.rows() * 16,
    }
}

/// Shared by reader, writer, and control. `closed` is set before the byte is written.
pub(crate) struct Wake {
    closed: AtomicBool,
    read: PipeReader,
}

impl Wake {
    pub(crate) fn new(read: PipeReader) -> Arc<Wake> {
        Arc::new(Wake {
            closed: AtomicBool::new(false),
            read,
        })
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// `EIO` after the last slave close is the hangup: EOF.
pub(crate) fn map_read(e: io::Error) -> io::Result<usize> {
    if e.raw_os_error() == Some(libc::EIO) {
        Ok(0)
    } else {
        Err(e)
    }
}

fn closed_before_eof() -> io::Error {
    io::Error::other("pty closed before eof")
}

pub(crate) struct MasterReader {
    master: OwnedFd,
    wake: Arc<Wake>,
    after_wake: Option<u64>,
    /// Runs between a `poll` that saw the master ready and the `read` (tests: F16).
    #[cfg(test)]
    pub(crate) after_poll: Option<Box<dyn FnMut() + Send>>,
}

impl MasterReader {
    pub(crate) fn new(master: OwnedFd, wake: Arc<Wake>) -> MasterReader {
        MasterReader {
            master,
            wake,
            after_wake: None,
            #[cfg(test)]
            after_poll: None,
        }
    }

    fn arm(&mut self) {
        if self.after_wake.is_none() {
            self.after_wake = Some(0);
        }
    }
}

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            if self.wake.closed() {
                self.arm();
            }
            if self.after_wake.is_some_and(|n| n >= AFTER_WAKE_CAP) {
                return Err(closed_before_eof());
            }
            let (m, w) = {
                let mut fds = [
                    PollFd::new(&self.master, PollFlags::IN),
                    PollFd::new(&self.wake.read, PollFlags::IN),
                ];
                let timeout = self.after_wake.map(|_| &zero);
                match poll(&mut fds, timeout) {
                    Ok(_) => {}
                    Err(Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                (fds[0].revents(), fds[1].revents())
            };
            if !w.is_empty() {
                self.arm();
            }
            if m.contains(PollFlags::NVAL) {
                return Err(io::Error::other("pty master invalid"));
            }
            if m.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                let max = match self.after_wake {
                    Some(n) => {
                        let left = usize::try_from(AFTER_WAKE_CAP - n).unwrap_or(usize::MAX);
                        buf.len().min(left)
                    }
                    None => buf.len(),
                };
                #[cfg(test)]
                if let Some(f) = self.after_poll.as_mut() {
                    f();
                }
                match rustix::io::read(&self.master, &mut buf[..max]) {
                    Ok(0) => return Ok(0),
                    Ok(n) => {
                        if let Some(done) = self.after_wake.as_mut() {
                            *done += n as u64;
                        }
                        return Ok(n);
                    }
                    Err(Errno::AGAIN | Errno::INTR) => continue,
                    Err(e) => return map_read(e.into()),
                }
            }
            if self.after_wake.is_some() {
                return Err(closed_before_eof());
            }
        }
    }
}

pub(crate) struct MasterWriter {
    master: OwnedFd,
    wake: Arc<Wake>,
    /// The error that ended the previous call after progress.
    pending: Option<io::Error>,
}

impl MasterWriter {
    pub(crate) fn new(master: OwnedFd, wake: Arc<Wake>) -> MasterWriter {
        MasterWriter {
            master,
            wake,
            pending: None,
        }
    }

    /// After progress: the count now, the error on the next call.
    fn end(&mut self, written: usize, e: io::Error) -> io::Result<usize> {
        if written > 0 {
            self.pending = Some(e);
            Ok(written)
        } else {
            Err(e)
        }
    }
}

fn pty_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "pty closed")
}

impl Write for MasterWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(e) = self.pending.take() {
            return Err(e);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        loop {
            if self.wake.closed() {
                return if written > 0 {
                    Ok(written)
                } else {
                    Err(pty_closed())
                };
            }
            match rustix::io::write(&self.master, &buf[written..]) {
                Ok(0) => return self.end(written, io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    written += n;
                    if written == buf.len() {
                        return Ok(written);
                    }
                    continue;
                }
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => {}
                Err(e) => return self.end(written, e.into()),
            }
            #[cfg(any(test, feature = "fake"))]
            WRITER_POLLS.fetch_add(1, Ordering::AcqRel);
            let (w, m) = loop {
                let mut fds = [
                    PollFd::new(&self.wake.read, PollFlags::IN),
                    PollFd::new(&self.master, PollFlags::OUT),
                ];
                match poll(&mut fds, None) {
                    Ok(_) => break (fds[0].revents(), fds[1].revents()),
                    Err(Errno::INTR) => {}
                    Err(e) => return self.end(written, e.into()),
                }
            };
            if !w.is_empty() {
                return if written > 0 {
                    Ok(written)
                } else {
                    Err(pty_closed())
                };
            }
            if m.contains(PollFlags::NVAL) {
                return self.end(written, io::Error::other("pty master invalid"));
            }
            // OUT, HUP, or ERR: retry the write (a hangup then fails `EIO`).
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MasterWriter {
    fn drop(&mut self) {
        #[cfg(test)]
        super::trace::record("input_drop");
    }
}

pub(crate) struct UnixControl {
    master: OwnedFd,
    wake: Arc<Wake>,
    wake_write: Option<PipeWriter>,
}

impl UnixControl {
    pub(crate) fn new(master: OwnedFd, wake: Arc<Wake>, wake_write: PipeWriter) -> UnixControl {
        UnixControl {
            master,
            wake,
            wake_write: Some(wake_write),
        }
    }

    /// Sets the flag, then writes the byte once on the nonblocking write end.
    fn fire(&mut self) {
        self.wake.closed.store(true, Ordering::Release);
        if let Some(w) = self.wake_write.take() {
            for _ in 0..=WAKE_RETRIES {
                match rustix::io::write(&w, b"x") {
                    Err(Errno::INTR) => {}
                    // `AGAIN`: a byte is already there; any other result ends the attempt.
                    _ => break,
                }
            }
        }
    }
}

impl PtyControl for UnixControl {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        tcsetwinsize(&self.master, winsize(size)).map_err(Into::into)
    }

    fn close(mut self: Box<Self>, _deadline: Instant) -> CloseReport {
        #[cfg(test)]
        super::trace::record("control_close");
        let t = Instant::now();
        self.fire();
        drop(self);
        CloseReport {
            elapsed: Some(t.elapsed()),
        }
    }
}

impl Drop for UnixControl {
    fn drop(&mut self) {
        self.fire();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::sync::mpsc;
    use std::time::Duration;

    use rustix::fs::{Mode, OFlags};
    use rustix::pty::OpenptFlags;
    use rustix::termios::{OptionalActions, QueueSelector, tcflush, tcgetattr, tcsetattr};

    use super::*;
    use crate::unix::tests::serial;

    const BOUND: Duration = Duration::from_secs(10);

    struct Pair {
        master: OwnedFd,
        slave: OwnedFd,
    }

    fn pair() -> Pair {
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        rustix::pty::grantpt(&master).unwrap();
        rustix::pty::unlockpt(&master).unwrap();
        let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let fl = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, fl | OFlags::NONBLOCK).unwrap();
        Pair { master, slave }
    }

    fn raw(slave: &OwnedFd) {
        let mut t = tcgetattr(slave).unwrap();
        t.make_raw();
        tcsetattr(slave, OptionalActions::Now, &t).unwrap();
    }

    fn wake() -> (Arc<Wake>, PipeWriter) {
        let (r, w) = std::io::pipe().unwrap();
        let fl = rustix::fs::fcntl_getfl(&w).unwrap();
        rustix::fs::fcntl_setfl(&w, fl | OFlags::NONBLOCK).unwrap();
        (Wake::new(r), w)
    }

    fn dup(fd: &OwnedFd) -> OwnedFd {
        rustix::io::fcntl_dupfd_cloexec(fd, 0).unwrap()
    }

    /// Reader, writer, and control over duplicates of `master`.
    fn ends(master: &OwnedFd) -> (MasterReader, MasterWriter, UnixControl) {
        let (wake, w) = wake();
        (
            MasterReader::new(dup(master), wake.clone()),
            MasterWriter::new(dup(master), wake.clone()),
            UnixControl::new(dup(master), wake, w),
        )
    }

    fn file_of(tag: &str, len: usize) -> (OwnedFd, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("chaldea-io-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("src-{len}"));
        std::fs::write(&path, vec![b'x'; len]).unwrap();
        (OwnedFd::from(std::fs::File::open(&path).unwrap()), dir)
    }

    fn read_all(r: &mut MasterReader) -> (usize, io::Result<usize>) {
        let mut buf = vec![0; 64 * 1024];
        let mut total = 0;
        loop {
            match r.read(&mut buf) {
                Ok(0) => return (total, Ok(0)),
                Ok(n) => total += n,
                Err(e) => return (total, Err(e)),
            }
        }
    }

    fn polls() -> u64 {
        WRITER_POLLS.load(Ordering::Acquire)
    }

    /// Waits until the writer has entered its full-queue `poll` since `before`.
    fn wait_polls(before: u64) {
        let end = Instant::now() + BOUND;
        while polls() <= before {
            assert!(Instant::now() < end, "the writer never entered its poll");
            std::thread::yield_now();
        }
    }

    #[test]
    fn map_read_turns_eio_into_eof() {
        assert_eq!(
            map_read(io::Error::from_raw_os_error(libc::EIO)).unwrap(),
            0
        );
        for errno in [libc::EAGAIN, libc::EBADF, libc::ENXIO] {
            let e = map_read(io::Error::from_raw_os_error(errno)).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn wake_caps_a_readable_source() {
        let (fd, dir) = file_of("cap", 3 << 20);
        let (w, wake_w) = wake();
        let mut control = MasterReader::new(fd, w);
        let (n, end) = read_all(&mut control);
        assert_eq!(n, 3 << 20);
        assert_eq!(end.unwrap(), 0);
        drop(wake_w);

        // A second open: a duplicate would share the offset, now at EOF.
        let fd = OwnedFd::from(std::fs::File::open(dir.join(format!("src-{}", 3 << 20))).unwrap());
        let (w, wake_w) = wake();
        let ctl = UnixControl::new(dup(&fd), w.clone(), wake_w);
        let mut reader = MasterReader::new(fd, w);
        Box::new(ctl).close(Instant::now());
        let (n, end) = read_all(&mut reader);
        assert_eq!(n, 1 << 20);
        assert_eq!(end.unwrap_err().to_string(), "pty closed before eof");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wake_when_master_and_wake_are_both_ready_arms_the_cap() {
        let (fd, dir) = file_of("both", 3 << 20);
        let (w, mut wake_w) = wake();
        // A byte on the wake pipe without the flag: only `poll` sees it.
        wake_w.write_all(b"x").unwrap();
        let mut reader = MasterReader::new(fd, w);
        let (n, end) = read_all(&mut reader);
        assert_eq!(n, 1 << 20);
        assert!(end.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flush_between_poll_and_read_does_not_block() {
        let _s = serial();
        let p = pair();
        let (mut reader, _input, control) = ends(&p.master);
        let slave = dup(&p.slave);
        let mut fired = false;
        reader.after_poll = Some(Box::new(move || {
            if !fired {
                fired = true;
                tcflush(&slave, QueueSelector::OFlush).unwrap();
            }
        }));
        rustix::io::write(&p.slave, b"abc").unwrap();
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            tx.send(reader.read(&mut buf)).unwrap();
        });
        // The flushed read did not return data or EOF; it is back in `poll`.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        Box::new(control).close(Instant::now());
        let r = rx.recv_timeout(BOUND).unwrap();
        assert_eq!(r.unwrap_err().to_string(), "pty closed before eof");
        t.join().unwrap();
    }

    #[test]
    fn wake_ends_the_reader_with_a_slave_open() {
        let _s = serial();
        let p = pair();
        let (mut reader, _input, control) = ends(&p.master);
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            tx.send(reader.read(&mut buf)).unwrap();
        });
        let t0 = Instant::now();
        Box::new(control).close(Instant::now());
        let r = rx.recv_timeout(BOUND).unwrap();
        assert!(r.is_err(), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(1));
        t.join().unwrap();
        drop(p.slave);
    }

    #[test]
    fn dropping_input_writes_nothing() {
        let _s = serial();
        let p = pair();
        let (_reader, mut input, _control) = ends(&p.master);
        let slave_readable = |ms: i32| {
            let mut fds = [PollFd::new(&p.slave, PollFlags::IN)];
            let t = Timespec {
                tv_sec: 0,
                tv_nsec: i64::from(ms) * 1_000_000,
            };
            poll(&mut fds, Some(&t)).unwrap() > 0
        };
        // Positive control: the bytes portable-pty's writer sends on drop are seen.
        input.write_all(b"\n\x04").unwrap();
        assert!(slave_readable(2_000));
        let mut buf = [0u8; 16];
        assert_eq!(rustix::io::read(&p.slave, &mut buf).unwrap(), 1);
        assert_eq!(rustix::io::read(&p.slave, &mut buf).unwrap(), 0);
        // The drop writes nothing.
        drop(input);
        assert!(!slave_readable(200));
    }

    /// A 64 KiB write in a thread against a raw slave nobody reads, once it is inside
    /// its full-queue `poll`.
    fn blocked_write(
        p: &Pair,
        input: MasterWriter,
    ) -> (
        mpsc::Receiver<(io::Result<usize>, MasterWriter)>,
        std::thread::JoinHandle<()>,
    ) {
        raw(&p.slave);
        let before = polls();
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut input = input;
            let r = input.write(&[b'a'; 64 * 1024]);
            tx.send((r, input)).unwrap();
        });
        wait_polls(before);
        (rx, t)
    }

    #[test]
    fn blocked_write_ends_at_the_wake() {
        let _s = serial();
        let p = pair();
        let (_reader, input, control) = ends(&p.master);
        let (rx, t) = blocked_write(&p, input);
        let t0 = Instant::now();
        Box::new(control).close(Instant::now());
        let (r, _) = rx.recv_timeout(BOUND).unwrap();
        let n = r.unwrap();
        assert!(n > 0 && n < 64 * 1024, "{n}");
        assert!(t0.elapsed() < Duration::from_secs(1));
        t.join().unwrap();
    }

    #[test]
    fn hangup_in_poll_ends_the_write() {
        let _s = serial();
        let p = pair();
        let (_reader, input, _control) = ends(&p.master);
        let (rx, t) = blocked_write(&p, input);
        let before = polls();
        drop(p.slave);
        let (r, mut input) = rx.recv_timeout(BOUND).unwrap();
        let n = r.unwrap();
        assert!(n > 0 && n < 64 * 1024, "{n}");
        let e = input.write(b"more").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EIO));
        // No spinning: at most the one `poll` that saw the hangup.
        assert!(polls() - before <= 1, "{} polls", polls() - before);
        t.join().unwrap();
    }

    #[test]
    fn wake_is_checked_between_writes() {
        let _s = serial();
        let p = pair();
        raw(&p.slave);
        let (_reader, mut input, control) = ends(&p.master);
        let slave = dup(&p.slave);
        let (got_tx, got_rx) = mpsc::channel();
        let drain = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut total = 0;
            let mut told = false;
            loop {
                match rustix::io::read(&slave, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => total += n,
                }
                if total >= 64 * 1024 && !told {
                    told = true;
                    got_tx.send(()).unwrap();
                }
            }
        });
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            tx.send(input.write(&vec![b'b'; 64 << 20])).unwrap();
        });
        got_rx.recv_timeout(BOUND).unwrap();
        Box::new(control).close(Instant::now());
        let n = rx.recv_timeout(BOUND).unwrap().unwrap();
        assert!((64 * 1024..64 << 20).contains(&n), "{n}");
        t.join().unwrap();
        drop(p.slave);
        drop(p.master);
        // The drain thread ends at the hangup once every master fd is closed.
        drop(_reader);
        drain.join().unwrap();
    }

    #[test]
    fn error_after_progress_returns_the_count_then_the_error() {
        let _s = serial();
        let (r, w) = std::io::pipe().unwrap();
        let fl = rustix::fs::fcntl_getfl(&w).unwrap();
        rustix::fs::fcntl_setfl(&w, fl | OFlags::NONBLOCK).unwrap();
        let (wk, _wake_w) = wake();
        let input = MasterWriter::new(OwnedFd::from(w), wk);
        let before = polls();
        let (tx, rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut input = input;
            let res = input.write(&vec![b'c'; 1 << 20]);
            tx.send((res, input)).unwrap();
        });
        wait_polls(before);
        drop(r);
        let (res, mut input) = rx.recv_timeout(BOUND).unwrap();
        let n = res.unwrap();
        assert!(n > 0 && n < 1 << 20, "{n}");
        let e = input.write(b"more").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPIPE));
        // Nothing is pending after the error was returned.
        assert_eq!(input.write(b"").unwrap(), 0);
        t.join().unwrap();
    }
}
