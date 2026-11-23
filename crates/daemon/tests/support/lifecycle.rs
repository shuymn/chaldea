//! Fixtures shared by the lifecycle unit tests and the `chaldea` binary tests, included in
//! both through `#[path]`; it names the crate as `chaldea_daemon` in both.

#![allow(dead_code, reason = "each test binary uses a subset")]
#![allow(clippy::missing_panics_doc, reason = "test support")]

use std::path::Path;
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_daemon::lifecycle::names;
use chaldea_daemon::store::{RandomIds, Store, StoreConfig, SystemClock};

/// A port nothing listens on right now (bound to `127.0.0.1:0`, then released).
#[must_use]
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The saved owner token (without its newline).
#[must_use]
pub fn token_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(names::OWNER_TOKEN))
        .unwrap()
        .trim_end()
        .to_owned()
}

/// Kills `child` and reaps it within 10 s (a process that cannot be reaped is reported, not
/// waited for).
pub fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("pid {} was not reaped within 10 s of its kill", child.id());
}

/// Opens the daemon's database directly (the daemon must not be running).
#[must_use]
pub fn open_store(dir: &Path) -> Store {
    Store::open(StoreConfig {
        path: dir.join(names::DB),
        readers: 2,
        writer_queue: 16,
        busy_timeout: Duration::from_secs(5),
        clock: Arc::new(SystemClock),
        ids: Arc::new(RandomIds),
    })
    .unwrap()
}
