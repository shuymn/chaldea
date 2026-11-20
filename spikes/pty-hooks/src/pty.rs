//! PTY I/O (§4.4): one reader thread draining output into `pty-output.bin`
//! and the VT; one writer thread owning the writer, fed by a channel shared
//! by input and VT replies so they stay ordered.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::MasterPty;

use crate::vt::{QueryLog, Vt};

pub enum WriteMsg {
    /// Written back-to-back without interleaving other messages (a whole paste).
    Data {
        chunks: Vec<Vec<u8>>,
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
}

impl WriterTx {
    pub fn send(&self, m: WriteMsg) -> bool {
        self.tx.send(m).is_ok()
    }

    /// Enqueues `chunks` as one unit and waits until written or `deadline`.
    pub fn write_wait(&self, chunks: Vec<Vec<u8>>, deadline: Instant) -> Result<(), String> {
        let (dtx, drx) = mpsc::channel();
        if !self.send(WriteMsg::Data {
            chunks,
            done: Some(dtx),
        }) {
            return Err("writer closed".into());
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        match drx.recv_timeout(wait) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => Err("write_timeout".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("writer gone".into()),
        }
    }
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
    pub fn start(
        master: Box<dyn MasterPty + Send>,
        out_file: &Path,
        cols: u16,
        rows: u16,
        responder: bool,
        t0: Instant,
    ) -> Result<PtyIo, String> {
        let mut reader = master
            .try_clone_reader()
            .map_err(|e| format!("reader: {e}"))?;
        let mut writer = master.take_writer().map_err(|e| format!("writer: {e}"))?;
        let (tx, rx): (Sender<WriteMsg>, Receiver<WriteMsg>) = mpsc::channel();
        let written = Arc::new(AtomicU64::new(0));
        let wtx = WriterTx {
            tx,
            written: written.clone(),
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
        let mut out =
            std::fs::File::create(out_file).map_err(|e| format!("{}: {e}", out_file.display()))?;

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
                            vt2.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .feed(&buf[..n], at);
                        }
                    }
                }
                let _ = out.flush();
                eof2.store(true, Ordering::SeqCst);
            })
            .map_err(|e| e.to_string())?;

        let log2 = log.clone();
        let writer_join = std::thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
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
                        WriteMsg::Data { chunks, done } => {
                            let mut r = Ok(());
                            for c in &chunks {
                                if let Err(e) = write(&mut writer, c) {
                                    r = Err(e.to_string());
                                    break;
                                }
                            }
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
                            let mut l = log2.lock().unwrap_or_else(|e| e.into_inner());
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
            })
            .map_err(|e| e.to_string())?;

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

    pub fn resize_hint(&self) -> Option<(u16, u16)> {
        self.master
            .as_ref()
            .and_then(|m| m.get_size().ok())
            .map(|s| (s.cols, s.rows))
    }

    /// Asks the writer thread to drop the writer (no-op if it is blocked).
    pub fn close_writer(&self) {
        self.tx.send(WriteMsg::Close);
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
