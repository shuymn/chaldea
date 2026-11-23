//! PR8-CL-20 (the subscriber's writer redacts every part of a line) and PR8-CL-26 (rotation).

use std::time::Duration;

use super::super::log::{LogFile, LogTarget, Logger, QueueWriter, TestLog, diag};
use super::super::names;

const TOKEN_MSG: &str = "chd_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const TOKEN_FIELD: &str = "chdt_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
const TOKEN_SPAN: &str = "chdp_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

/// An event with distinct secrets in the message, a field, and a span field, through the
/// real `QueueWriter` into a captured target.
#[test]
fn redacts_message_fields_and_spans() {
    let captured = TestLog::default();
    let logger = Logger::start(LogTarget::Test(captured.clone()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(QueueWriter(Some(logger.clone())))
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("request", secret = TOKEN_SPAN);
        let _g = span.enter();
        tracing::info!(field = TOKEN_FIELD, "message {TOKEN_MSG}");
    });
    assert!(logger.flush(Duration::from_secs(5)));
    let text = captured.text();
    assert!(text.contains("message chd_<redacted>"), "{text}");
    assert!(text.contains("chdt_<redacted>"), "{text}");
    assert!(text.contains("chdp_<redacted>"), "{text}");
    for t in [TOKEN_MSG, TOKEN_FIELD, TOKEN_SPAN] {
        assert!(!text.contains(t), "{text}");
    }
}

#[test]
fn diag_redacts_and_replaces_controls() {
    let d = diag(&format!("a\x1b[31m{TOKEN_MSG}\tb\nc"));
    assert_eq!(d, "a [31mchd_<redacted> b\nc");
}

fn line(n: usize) -> String {
    format!("{n:08} {}\n", "x".repeat(100))
}

/// Past a 64 KiB test cap, `daemon.log.1` holds the old lines and a fresh `daemon.log` the
/// new ones; neither file exceeds the cap plus one line.
#[test]
fn rotates_at_the_cap() {
    const CAP: u64 = 64 * 1024;
    let d = tempfile::tempdir().unwrap();
    let mut f = LogFile::open(d.path().to_path_buf(), CAP).unwrap();
    let n = 1000;
    for i in 0..n {
        f.write(&line(i));
    }
    drop(f);
    let old = std::fs::read_to_string(d.path().join(names::LOG_OLD)).unwrap();
    let new = std::fs::read_to_string(d.path().join(names::LOG)).unwrap();
    let one = line(0).len() as u64;
    assert!(old.len() as u64 <= CAP + one && new.len() as u64 <= CAP + one);
    assert!(
        new.contains(&line(n - 1)),
        "the newest line is in daemon.log"
    );
    assert!(!new.contains(&line(0)));
    let first_new: usize = new[..8].parse().unwrap();
    assert!(
        old.contains(&line(first_new - 1)),
        "the line before is in daemon.log.1"
    );
}

/// `daemon.log.1` a directory: the rename fails, `daemon.log` never exceeds the cap plus one
/// line, and the lines are counted as dropped.
#[test]
fn rotation_failure_stops_writing() {
    const CAP: u64 = 64 * 1024;
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join(names::LOG_OLD)).unwrap();
    let mut f = LogFile::open(d.path().to_path_buf(), CAP).unwrap();
    for i in 0..2000 {
        f.write(&line(i));
    }
    let size = std::fs::metadata(d.path().join(names::LOG)).unwrap().len();
    assert!(size <= CAP + line(0).len() as u64, "{size}");
    assert!(f.dropped() > 0);
    assert!(d.path().join(names::LOG_OLD).is_dir());
}
