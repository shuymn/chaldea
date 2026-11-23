//! A process killed before or after `COMMIT` (PR6-CL-16). The test binary re-runs itself as a
//! child that commits jobs, prints a marker, and blocks; the parent kills it and reopens.

mod support;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chaldea_core::ids::{IdempotencyKey, StreamId};
use chaldea_daemon::lifecycle::spawn_locked;
use chaldea_daemon::store::{NewEvent, Store, prepare_content};
use support::{FixedClock, T0, config, raw_unknown, registered, soon, thread};

const DIR_VAR: &str = "CHALDEA_CRASH_DIR";
const MODE_VAR: &str = "CHALDEA_CRASH_MODE";
const MARKER: &str = "CHALDEA-CRASH-MARKER";
/// Jobs the child commits before the marker.
const COMMITTED: u64 = 3;
/// Events of the large job (each holds a 1 MiB blob, more than the page cache, so its pages
/// reach the WAL before `COMMIT`).
const LARGE: u128 = 5;

fn large_job() -> Vec<NewEvent> {
    (0..LARGE)
        .map(|n| {
            let byte = u8::try_from(n).unwrap();
            let p = prepare_content(vec![byte; 1024 * 1024], "application/octet-stream").unwrap();
            let mut e = raw_unknown(StreamId::Host, &format!("large:{n}"), p.content);
            e.blobs = p.blob.into_iter().collect();
            e
        })
        .collect()
}

/// The child: commits `COMMITTED` jobs, then prints the marker inside a large job
/// (`before`) or after one more job committed (`after`), and blocks until killed.
#[test]
fn crash_child() {
    let (Ok(dir), Ok(mode)) = (std::env::var(DIR_VAR), std::env::var(MODE_VAR)) else {
        return;
    };
    let store = Store::open(config(
        PathBuf::from(dir).join("chaldea.db"),
        FixedClock::new(T0),
    ))
    .unwrap();
    for n in 0..u128::from(COMMITTED) {
        let e = registered(thread(n + 1), false);
        store
            .submit(soon(), move |tx| Ok(tx.append(vec![e])?))
            .unwrap()
            .blocking_wait()
            .unwrap();
    }
    let block = || -> ! {
        let (_tx, rx) = mpsc::channel::<()>();
        let _ = rx.recv_timeout(Duration::from_secs(120));
        panic!("the parent did not kill the child");
    };
    if mode == "before" {
        let events = large_job();
        let _ticket: chaldea_daemon::store::WriteTicket<()> = store
            .submit(soon(), move |tx| {
                tx.append(events)?;
                println!("{MARKER}");
                block();
            })
            .unwrap();
        block();
    } else {
        let events = large_job();
        store
            .submit(soon(), move |tx| Ok(tx.append(events)?))
            .unwrap()
            .blocking_wait()
            .unwrap();
        println!("{MARKER}");
        block();
    }
}

/// Runs the child in `mode`, kills it at the marker, and returns the host head and whether the
/// large job's events are there after reopening.
fn crash(mode: &str) -> (u64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_locked(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
            .env(DIR_VAR, dir.path())
            .env(MODE_VAR, mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()),
    )
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains(MARKER) => break,
            Ok(_) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("no marker from the child ({mode}): {e}");
            }
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    reopen_and_count(dir.path())
}

fn reopen_and_count(dir: &Path) -> (u64, usize) {
    let store = Store::open(config(dir.join("chaldea.db"), FixedClock::new(T0))).unwrap();
    let result = store
        .read(soon(), |r| {
            let head = r.head(StreamId::Host, 0)?.head_seq;
            let mut large = 0;
            for n in 0..LARGE {
                if r.event_by_key(StreamId::Host, &IdempotencyKey(format!("large:{n}")))?
                    .is_some()
                {
                    large += 1;
                }
            }
            Ok((head, large))
        })
        .unwrap();
    let report = store.shutdown(Instant::now() + Duration::from_secs(20));
    assert!(report.writer_exited, "{report:?}");
    result
}

/// Killed inside a job before `COMMIT`: none of the job, all earlier jobs.
#[test]
fn kill_before_commit_leaves_no_partial_job() {
    assert_eq!(crash("before"), (COMMITTED, 0));
}

/// Killed after `COMMIT` returned: the whole job is there.
#[test]
fn kill_after_commit_keeps_the_job() {
    assert_eq!(
        crash("after"),
        (
            COMMITTED + u64::try_from(LARGE).unwrap(),
            usize::try_from(LARGE).unwrap()
        )
    );
}
