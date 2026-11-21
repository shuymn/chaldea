//! PTY I/O (§4.4): one reader thread draining output into `pty-output.bin`
//! and the VT; one writer thread owning the writer, fed by a channel shared
//! by input and VT replies so they stay ordered.
//!
//! Gated input carries the gate generation it was admitted under and its
//! deadline; the writer re-checks both immediately before the first byte, so
//! a paste queued behind a slow write never lands after DECRST 2004 or after
//! its caller gave up.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::MasterPty;

use crate::vt::{QueryLog, Vt};

/// DECSET 2004 as seen by the reader; `epoch` changes on every transition.
/// `revoked` is sticky: once binding becomes ambiguous the gate never opens again.
/// `revocations` changes only on revocation, so key input (`GateMode::Keys`)
/// survives DECSET 2004 toggles.
#[derive(Default)]
pub struct GateState {
    pub open: AtomicBool,
    pub epoch: AtomicU64,
    pub revoked: AtomicBool,
    pub revocations: AtomicU64,
}

impl GateState {
    pub fn set(&self, open: bool) {
        if self.open.swap(open, Ordering::SeqCst) != open {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Closes the gate for good and bumps the generation, so a paste admitted
    /// earlier and still queued is dropped before its first byte.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.revocations.fetch_add(1, Ordering::SeqCst);
    }
    /// The current generation if the gate is open.
    pub fn admit(&self) -> Option<u64> {
        let g = self.epoch.load(Ordering::SeqCst);
        // A transition racing this read bumps `epoch`, so the writer's re-check fails safe.
        (self.open.load(Ordering::SeqCst) && !self.revoked.load(Ordering::SeqCst)).then_some(g)
    }
    /// The current revocation epoch unless revoked (key input ignores DECSET 2004).
    pub fn admit_keys(&self) -> Option<u64> {
        let g = self.revocations.load(Ordering::SeqCst);
        (!self.revoked.load(Ordering::SeqCst)).then_some(g)
    }
    fn admits(&self, mode: GateMode, epoch: u64) -> bool {
        match mode {
            GateMode::Paste => self.admit() == Some(epoch),
            GateMode::Keys => self.admit_keys() == Some(epoch),
        }
    }
}

/// What the writer re-checks before the first byte: `Paste` needs the same
/// DECSET 2004 generation it was admitted under; `Keys` (Codex `win_keys`,
/// which ignores bracketed paste) needs only that the launch was not revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateMode {
    #[default]
    Paste,
    Keys,
}

/// Conditions re-checked by the writer just before the first byte.
pub struct Guard {
    pub gate: Option<(Arc<GateState>, u64)>,
    pub mode: GateMode,
    pub deadline: Instant,
}

const PENDING: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

pub enum WriteMsg {
    /// Written back-to-back without interleaving other messages (a whole paste).
    Data {
        chunks: Vec<Vec<u8>>,
        guard: Option<Guard>,
        state: Arc<AtomicU8>,
        done: Option<Sender<Result<(), String>>>,
    },
    Reply {
        bytes: Vec<u8>,
        log_id: usize,
        query_at: Instant,
    },
    Close,
}

#[derive(Clone)]
pub struct WriterTx {
    tx: Sender<WriteMsg>,
    /// Bytes the writer thread has handed to the PTY.
    pub written: Arc<AtomicU64>,
    pub gate: Arc<GateState>,
}

impl WriterTx {
    pub fn send(&self, m: WriteMsg) -> bool {
        self.tx.send(m).is_ok()
    }

    /// Ungated write (VT replies, keyboard/probe keys); waits until written or `deadline`.
    pub fn write_wait(&self, chunks: Vec<Vec<u8>>, deadline: Instant) -> Result<(), String> {
        self.write_guarded(
            chunks,
            Guard {
                gate: None,
                mode: GateMode::Paste,
                deadline,
            },
        )
    }

    /// Enqueues `chunks` as one unit. Errors: `expired_before_start` (never
    /// written), `gate_closed_before_start` (never written), `write_timeout`
    /// (started but not finished by the deadline), or an I/O error.
    pub fn write_guarded(&self, chunks: Vec<Vec<u8>>, guard: Guard) -> Result<(), String> {
        let deadline = guard.deadline;
        let state = Arc::new(AtomicU8::new(PENDING));
        let (dtx, drx) = mpsc::channel();
        if !self.send(WriteMsg::Data {
            chunks,
            guard: Some(guard),
            state: state.clone(),
            done: Some(dtx),
        }) {
            return Err("writer closed".into());
        }
        match drx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Cancel if the writer has not started; it then never writes it.
                if state
                    .compare_exchange(PENDING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    Err("expired_before_start".into())
                } else {
                    Err("write_timeout".into())
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("writer gone".into()),
        }
    }
}

/// The writer thread body (separate for tests).
pub fn writer_loop(
    rx: Receiver<WriteMsg>,
    mut writer: Box<dyn Write + Send>,
    written: Arc<AtomicU64>,
    log: QueryLog,
) {
    let write = |w: &mut Box<dyn Write + Send>, b: &[u8]| -> std::io::Result<()> {
        let mut off = 0;
        while off < b.len() {
            let n = w.write(&b[off..])?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            off += n;
            written.fetch_add(n as u64, Ordering::SeqCst);
        }
        w.flush()
    };
    while let Ok(m) = rx.recv() {
        match m {
            WriteMsg::Data {
                chunks,
                guard,
                state,
                done,
            } => {
                if state
                    .compare_exchange(PENDING, STARTED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    continue; // cancelled by its caller: never written
                }
                let pre = guard.as_ref().and_then(|g| {
                    if Instant::now() >= g.deadline {
                        Some("expired_before_start")
                    } else if g
                        .gate
                        .as_ref()
                        .is_some_and(|(s, epoch)| !s.admits(g.mode, *epoch))
                    {
                        Some("gate_closed_before_start")
                    } else {
                        None
                    }
                });
                let r = match pre {
                    Some(e) => Err(e.to_string()),
                    None => chunks
                        .iter()
                        .try_for_each(|c| write(&mut writer, c))
                        .map_err(|e| e.to_string()),
                };
                if let Some(d) = done {
                    let _ = d.send(r);
                }
            }
            WriteMsg::Reply {
                bytes,
                log_id,
                query_at,
            } => {
                let ok = write(&mut writer, &bytes).is_ok();
                let mut l = log.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(e) = l.get_mut(log_id) {
                    e.harness_wrote = ok;
                    e.latency_ms = Some(query_at.elapsed().as_secs_f64() * 1000.0);
                }
            }
            WriteMsg::Close => break,
        }
    }
    // Dropping the writer here, after the tree is dead (teardown order, §4.7).
    drop(writer);
}

/// A writer thread over `w` with a fresh gate (test rigs for code that
/// drives a `WriterTx`); send `WriteMsg::Close` to end it.
pub fn spawn_writer(w: Box<dyn Write + Send>) -> (WriterTx, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let written = Arc::new(AtomicU64::new(0));
    let wtx = WriterTx {
        tx,
        written: written.clone(),
        gate: Arc::new(GateState::default()),
    };
    let j =
        std::thread::spawn(move || writer_loop(rx, w, written, Arc::new(Mutex::new(Vec::new()))));
    (wtx, j)
}

pub struct PtyIo {
    pub vt: Arc<Mutex<Vt>>,
    pub tx: WriterTx,
    pub log: QueryLog,
    pub reader_eof: Arc<AtomicBool>,
    pub saw_win32_input_mode: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    master: Option<Box<dyn MasterPty + Send>>,
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl PtyIo {
    /// Every fallible step runs before the writer is taken. If the last step
    /// (the writer thread) fails, `kill_first` kills the tree before the Unix
    /// writer drops (its drop writes `\n` + VEOF to the PTY).
    pub fn start(
        master: Box<dyn MasterPty + Send>,
        mut out: std::fs::File,
        cols: u16,
        rows: u16,
        responder: bool,
        t0: Instant,
        kill_first: &dyn Fn(),
    ) -> Result<PtyIo, String> {
        // Every setup error kills the tree before `master` (owned here) drops,
        // so ConPTY is never closed under a live, undrained child.
        let fail = |e: String| -> Result<PtyIo, String> {
            kill_first();
            Err(e)
        };
        let mut reader = match master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => return fail(format!("reader: {e}")),
        };
        let (tx, rx): (Sender<WriteMsg>, Receiver<WriteMsg>) = mpsc::channel();
        let written = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(GateState::default());
        let wtx = WriterTx {
            tx,
            written: written.clone(),
            gate: gate.clone(),
        };
        let log: QueryLog = Arc::new(Mutex::new(Vec::new()));
        let vt = Arc::new(Mutex::new(Vt::new(
            cols,
            rows,
            wtx.clone(),
            responder,
            log.clone(),
            t0,
        )));

        let reader_eof = Arc::new(AtomicBool::new(false));
        let saw = Arc::new(AtomicBool::new(false));
        let (vt2, eof2, saw2) = (vt.clone(), reader_eof.clone(), saw.clone());
        let reader_join = std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                let mut tail: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let at = Instant::now();
                            let _ = out.write_all(&buf[..n]);
                            tail.extend_from_slice(&buf[..n]);
                            if contains(&tail, b"\x1b[?9001h") {
                                saw2.store(true, Ordering::SeqCst);
                            }
                            let keep = tail.len().saturating_sub(8);
                            tail.drain(..keep);
                            let mut v = vt2.lock().unwrap_or_else(|e| e.into_inner());
                            v.feed(&buf[..n], at);
                            gate.set(v.bracketed_paste());
                        }
                    }
                }
                let _ = out.flush();
                eof2.store(true, Ordering::SeqCst);
            });
        let reader_join = match reader_join {
            Ok(j) => j,
            Err(e) => return fail(format!("reader thread: {e}")),
        };

        let writer = match master.take_writer() {
            Ok(w) => w,
            Err(e) => return fail(format!("writer: {e}")),
        };
        let log2 = log.clone();
        let (wtx_slot, wrx) = mpsc::channel::<Box<dyn Write + Send>>();
        let spawned = std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                if let Ok(w) = wrx.recv() {
                    writer_loop(rx, w, written, log2);
                }
            });
        let writer_join = match spawned {
            Ok(j) => {
                let _ = wtx_slot.send(writer);
                j
            }
            Err(e) => {
                kill_first();
                drop(writer);
                return Err(e.to_string());
            }
        };

        Ok(PtyIo {
            vt,
            tx: wtx,
            log,
            reader_eof,
            saw_win32_input_mode: saw,
            reader: Some(reader_join),
            writer: Some(writer_join),
            master: Some(master),
        })
    }

    /// Asks the writer thread to drop the writer (no-op if it is blocked).
    pub fn close_writer(&self) {
        self.tx.send(WriteMsg::Close);
    }

    /// Resizes the PTY (`ResizePseudoConsole` under the raw backend), then
    /// the VT grid and the size its responder reports.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let m = self.master.as_ref().ok_or("master closed")?;
        m.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;
        self.vt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(cols, rows);
        Ok(())
    }

    /// Closes the master (ConPTY on Windows) while the reader keeps draining.
    pub fn close_master(&mut self) {
        self.master.take();
    }

    /// Joins I/O threads within `bound`; blocked threads are abandoned, not joined.
    pub fn join_bounded(&mut self, bound: Duration) -> serde_json::Value {
        let end = Instant::now() + bound;
        let mut res = serde_json::Map::new();
        for (name, slot) in [("writer", &mut self.writer), ("reader", &mut self.reader)] {
            if let Some(h) = slot.take() {
                while !h.is_finished() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if h.is_finished() {
                    let _ = h.join();
                    res.insert(name.into(), "joined".into());
                } else {
                    res.insert(name.into(), "abandoned".into());
                }
            }
        }
        serde_json::Value::Object(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose first write blocks until released.
    struct Gated {
        release: Receiver<()>,
        first: bool,
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Gated {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.first {
                self.first = false;
                let _ = self.release.recv();
            }
            self.out.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Writer handle, release switch for the held writer, bytes written, writer thread.
    type Rig = (WriterTx, Sender<()>, Arc<Mutex<Vec<u8>>>, JoinHandle<()>);

    fn rig() -> Rig {
        let (tx, rx) = mpsc::channel();
        let (rel_tx, rel_rx) = mpsc::channel();
        let out = Arc::new(Mutex::new(Vec::new()));
        let w = Gated {
            release: rel_rx,
            first: true,
            out: out.clone(),
        };
        let written = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(GateState::default());
        gate.set(true);
        let wtx = WriterTx {
            tx,
            written: written.clone(),
            gate,
        };
        let j = std::thread::spawn(move || {
            writer_loop(rx, Box::new(w), written, Arc::new(Mutex::new(Vec::new())))
        });
        (wtx, rel_tx, out, j)
    }

    fn blocker(tx: &WriterTx) {
        tx.send(WriteMsg::Data {
            chunks: vec![b"A".to_vec()],
            guard: None,
            state: Arc::new(AtomicU8::new(PENDING)),
            done: None,
        });
    }

    /// M4: a gated paste that expires while queued behind a blocked write is
    /// reported as expired and never reaches the PTY.
    /// M6: a resize reaches the VT grid and the size the responder reports,
    /// not only the PTY: the snapshot after the resize has the new geometry
    /// and content drawn afterwards lands in it.
    #[test]
    fn resize_resizes_the_vt_grid() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let out = std::env::temp_dir().join(format!("chs-resize-{}", crate::util::run_id()));
        let mut io = PtyIo::start(
            pair.master,
            std::fs::File::create(&out).unwrap(),
            120,
            40,
            true,
            Instant::now(),
            &|| {},
        )
        .unwrap();
        io.resize(100, 30).unwrap();
        let wide = "x".repeat(110);
        {
            let mut vt = io.vt.lock().unwrap();
            vt.feed(format!("\x1b[H{wide}").as_bytes(), Instant::now());
            let s = vt.snapshot();
            assert_eq!((s.cols, s.rows), (100, 30));
            // 110 columns of text wrap at the new width.
            assert_eq!(s.lines[0], "x".repeat(100));
            assert_eq!(s.lines[1], "x".repeat(10));
            assert_eq!(vt.size(), (100, 30));
        }
        io.close_writer();
        io.close_master();
        drop(pair.slave);
        io.join_bounded(Duration::from_secs(2));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn expired_queued_paste_is_never_written() {
        let (tx, release, out, j) = rig();
        blocker(&tx);
        let epoch = tx.gate.admit().unwrap();
        let r = tx.write_guarded(
            vec![b"PASTE".to_vec()],
            Guard {
                gate: Some((tx.gate.clone(), epoch)),
                mode: GateMode::Paste,
                deadline: Instant::now() + Duration::from_millis(100),
            },
        );
        assert_eq!(r, Err("expired_before_start".into()));
        release.send(()).unwrap();
        tx.send(WriteMsg::Close);
        j.join().unwrap();
        assert_eq!(&*out.lock().unwrap(), b"A");
    }

    /// M4: DECRST 2004 between admission and the first byte drops the paste.
    #[test]
    fn gate_closed_after_admission_drops_paste() {
        let (tx, release, out, j) = rig();
        blocker(&tx);
        let epoch = tx.gate.admit().unwrap();
        let (dtx, drx) = mpsc::channel();
        tx.send(WriteMsg::Data {
            chunks: vec![b"PASTE".to_vec()],
            guard: Some(Guard {
                gate: Some((tx.gate.clone(), epoch)),
                mode: GateMode::Paste,
                deadline: Instant::now() + Duration::from_secs(10),
            }),
            state: Arc::new(AtomicU8::new(PENDING)),
            done: Some(dtx),
        });
        tx.gate.set(false);
        release.send(()).unwrap();
        assert_eq!(
            drx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err("gate_closed_before_start".into())
        );
        tx.send(WriteMsg::Close);
        j.join().unwrap();
        assert_eq!(&*out.lock().unwrap(), b"A");
    }

    /// F-1a: key input queued behind a write survives a DECSET 2004 toggle
    /// (Codex ignores bracketed paste), but a revocation still drops it.
    #[test]
    fn keys_mode_ignores_2004_but_not_revocation() {
        let send = |tx: &WriterTx, epoch: u64| {
            let (dtx, drx) = mpsc::channel();
            tx.send(WriteMsg::Data {
                chunks: vec![b"KEYS".to_vec()],
                guard: Some(Guard {
                    gate: Some((tx.gate.clone(), epoch)),
                    mode: GateMode::Keys,
                    deadline: Instant::now() + Duration::from_secs(10),
                }),
                state: Arc::new(AtomicU8::new(PENDING)),
                done: Some(dtx),
            });
            drx
        };
        let (tx, release, out, j) = rig();
        blocker(&tx);
        let drx = send(&tx, tx.gate.admit_keys().unwrap());
        tx.gate.set(false);
        tx.gate.set(true);
        release.send(()).unwrap();
        assert_eq!(drx.recv_timeout(Duration::from_secs(5)).unwrap(), Ok(()));
        assert_eq!(&*out.lock().unwrap(), b"AKEYS");
        tx.send(WriteMsg::Close);
        j.join().unwrap();

        let (tx, release, out, j) = rig();
        blocker(&tx);
        let drx = send(&tx, tx.gate.admit_keys().unwrap());
        tx.gate.revoke();
        release.send(()).unwrap();
        assert_eq!(
            drx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err("gate_closed_before_start".into())
        );
        assert!(tx.gate.admit_keys().is_none());
        tx.send(WriteMsg::Close);
        j.join().unwrap();
        assert_eq!(&*out.lock().unwrap(), b"A");
    }
}
