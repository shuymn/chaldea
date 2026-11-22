//! Helpers shared by the integration tests. Waits are bounded in seconds so they hold on
//! loaded CI runners; ordering uses `WriteHold` and `Probe`, never sleeps.

#![allow(dead_code, reason = "each test binary uses a subset")]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::{FakeChild, FakePty, FakeScript};
use chaldea_pty_runtime::{
    CloseBounds, Completion, Guard, Limits, Part, PtySize, Session, SessionConfig, SessionEvent,
    SpawnSpec, WriteOrigin, WriteRecord, WriteTicket, WriteUnit, completion_channel,
};

/// The bound of every wait for something that should happen.
pub const WAIT: Duration = Duration::from_secs(10);

pub fn soon() -> Instant {
    Instant::now() + WAIT
}

pub fn spec(cols: u16, rows: u16) -> SpawnSpec {
    SpawnSpec {
        argv: vec![OsString::from("/work/bin/agent")],
        cwd: PathBuf::from("/work/repo"),
        env: vec![(OsString::from("TERM"), OsString::from("xterm-256color"))],
        size: PtySize::new(cols, rows).unwrap(),
    }
}

/// Every `SessionEvent` the session emitted, in order.
#[derive(Clone, Default)]
pub struct Events(pub Arc<Mutex<Vec<SessionEvent>>>);

impl Events {
    pub fn all(&self) -> Vec<SessionEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, f: impl Fn(&SessionEvent) -> bool) -> usize {
        self.all().iter().filter(|e| f(e)).count()
    }

    /// Waits until an event matching `f` was emitted.
    pub fn wait(&self, f: impl Fn(&SessionEvent) -> bool) -> bool {
        wait_until(|| self.all().iter().any(&f))
    }
}

pub struct Rig {
    pub session: Session,
    pub child: FakeChild,
    pub events: Events,
    pub fake: FakePty,
}

pub fn rig() -> Rig {
    rig_with(FakeScript::default(), Limits::default(), 80, 24)
}

pub fn rig_limits(limits: Limits) -> Rig {
    rig_with(FakeScript::default(), limits, 80, 24)
}

pub fn rig_with(script: FakeScript, limits: Limits, cols: u16, rows: u16) -> Rig {
    let events = Events::default();
    let ev = events.clone();
    let mut config = SessionConfig::new(move |e| ev.0.lock().unwrap().push(e));
    config.limits = limits;
    let fake = FakePty::new();
    fake.script_next(script);
    let session = Session::spawn(&fake, &spec(cols, rows), config).unwrap();
    let child = fake.take_child().unwrap();
    Rig {
        session,
        child,
        events,
        fake,
    }
}

/// Polls `f` (a state check, not an ordering signal) until it holds or `WAIT` passes.
pub fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    let deadline = soon();
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits until the VT parsed at least `n` output bytes.
pub fn wait_parsed(s: &Session, n: u64) {
    assert!(
        wait_until(|| s.snapshot(soon()).is_ok_and(|x| x.output_offset >= n)),
        "VT did not parse {n} bytes"
    );
}

/// Emits `bytes` and waits until the VT parsed them.
pub fn emit_parsed(r: &Rig, bytes: &[u8]) {
    let before = r.session.snapshot(soon()).map_or(0, |s| s.output_offset);
    r.child.emit(bytes);
    wait_parsed(&r.session, before + bytes.len() as u64);
}

pub fn key(bytes: &[u8]) -> WriteUnit {
    WriteUnit::bytes(WriteOrigin::Key, bytes)
}

pub fn unit(origin: WriteOrigin, parts: Vec<Part>) -> WriteUnit {
    WriteUnit { origin, parts }
}

pub fn input_origin(n: u128) -> WriteOrigin {
    WriteOrigin::Input {
        input_id: chaldea_core::ids::InputId::from_u128(n),
    }
}

pub fn b(bytes: impl AsRef<[u8]>) -> Part {
    Part::Bytes(bytes.as_ref().to_vec())
}

pub fn pause(ms: u64) -> Part {
    Part::Pause(Duration::from_millis(ms))
}

pub fn open() -> Guard {
    Guard::until(soon())
}

/// A pending completion.
pub struct Pending {
    pub ticket: WriteTicket,
    rx: chaldea_pty_runtime::CompletionReceiver,
}

impl Pending {
    pub fn wait(&self) -> WriteRecord {
        self.rx
            .recv_deadline(soon())
            .expect("completion within WAIT")
    }

    pub fn try_now(&self) -> Option<WriteRecord> {
        self.rx.recv_deadline(Instant::now()).ok()
    }
}

pub fn submit(s: &Session, u: WriteUnit, g: Guard) -> Pending {
    let (done, rx) = completion_channel();
    let ticket = s.submit(u, g, done).expect("submit accepted");
    Pending { ticket, rx }
}

/// A completion that counts its calls and keeps the last record.
pub fn counting() -> (Completion, Arc<Mutex<Vec<WriteRecord>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (
        Box::new(move |r: &WriteRecord| s.lock().unwrap().push(r.clone())),
        seen,
    )
}

/// Short close budgets for tests that expect a step to run out.
pub fn short_bounds() -> CloseBounds {
    CloseBounds {
        kill: Duration::from_millis(300),
        input: Duration::from_millis(300),
        close: Duration::from_millis(300),
        drain: Duration::from_millis(300),
        join: Duration::from_millis(500),
    }
}

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
