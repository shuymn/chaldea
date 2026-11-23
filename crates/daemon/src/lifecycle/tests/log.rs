//! PR8-CL-20 (the subscriber's writer redacts every part of a line) and PR8-CL-26 (rotation).

use std::time::Duration;

use super::super::log::{LogFile, LogTarget, Logger, QueueWriter, TestLog, diag};
use super::super::names;
use super::super::ready::fail_line;

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
/// new ones; neither file exceeds the cap plus one line; nothing rotates below the cap.
#[test]
fn rotates_at_the_cap() {
    const CAP: u64 = 64 * 1024;
    let d = tempfile::tempdir().unwrap();
    let mut f = LogFile::open(d.path().to_path_buf(), CAP).unwrap();
    let one = line(0).len() as u64;
    // Lines up to the cap stay in one file, the first one included.
    let below = CAP / one;
    for i in 0..usize::try_from(below).unwrap() {
        f.write(&line(i));
    }
    assert!(
        !d.path().join(names::LOG_OLD).exists(),
        "rotated below the cap"
    );
    let n = 1000;
    for i in usize::try_from(below).unwrap()..n {
        f.write(&line(i));
    }
    drop(f);
    let old = std::fs::read_to_string(d.path().join(names::LOG_OLD)).unwrap();
    let new = std::fs::read_to_string(d.path().join(names::LOG)).unwrap();
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

// ---- Review round 1 ----

/// A rotation whose rename committed but whose new `daemon.log` could not be created recovers
/// on the next attempt by creating it, without renaming again.
#[test]
fn rotation_resumes_after_a_failed_reopen() {
    const CAP: u64 = 64 * 1024;
    let d = tempfile::tempdir().unwrap();
    let mut f = LogFile::open(d.path().to_path_buf(), CAP).unwrap();
    f.set_retry(Duration::ZERO);
    f.fail_reopen = 1;
    let mut i = 0;
    while !d.path().join(names::LOG_OLD).exists() {
        assert!(i < 10_000, "no rotation after {i} lines");
        f.write(&line(i));
        i += 1;
    }
    // The rename committed; the reopen failed, so this line was dropped.
    assert!(!d.path().join(names::LOG).exists());
    let marker = line(999_999);
    f.write(&marker);
    drop(f);
    let new = std::fs::read_to_string(d.path().join(names::LOG)).unwrap();
    assert!(new.contains(&marker), "{new}");
    assert!(
        new.contains("log lines dropped while rotation failed"),
        "{new}"
    );
    let old = std::fs::read_to_string(d.path().join(names::LOG_OLD)).unwrap();
    assert!(
        old.starts_with(&line(0)),
        "the rotated file is the original one"
    );
}

/// Bytes a failed write left in the file count toward the cap: the file never exceeds the
/// cap plus one line.
#[test]
fn partial_write_counts_toward_the_cap() {
    const CAP: u64 = 4 * 1024;
    let d = tempfile::tempdir().unwrap();
    let mut f = LogFile::open(d.path().to_path_buf(), CAP).unwrap();
    let one = line(0).len();
    f.partial_write = Some(one - 1);
    let mut i = 0;
    while !d.path().join(names::LOG_OLD).exists() {
        assert!(i < 10_000, "no rotation after {i} lines");
        f.write(&line(i));
        i += 1;
    }
    drop(f);
    // The file holding the partial line was rotated once.
    let size = std::fs::metadata(d.path().join(names::LOG_OLD))
        .unwrap()
        .len();
    assert!(size <= CAP + one as u64, "{size}");
}

/// The three secret forms are redacted in `daemon.log` itself, through the subscriber and
/// through the logger's own lines.
#[test]
fn secrets_are_redacted_in_daemon_log() {
    let d = tempfile::tempdir().unwrap();
    let file = LogFile::open(d.path().to_path_buf(), 1024 * 1024).unwrap();
    let logger = Logger::start(LogTarget::File(file));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(QueueWriter(Some(logger.clone())))
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("request", secret = TOKEN_SPAN);
        let _g = span.enter();
        tracing::info!(field = TOKEN_FIELD, "message {TOKEN_MSG}");
    });
    logger.info(&format!("direct {TOKEN_MSG} {TOKEN_FIELD} {TOKEN_SPAN}"));
    assert!(logger.flush(Duration::from_secs(5)));
    let text = std::fs::read_to_string(d.path().join(names::LOG)).unwrap();
    assert!(text.contains("message chd_<redacted>"), "{text}");
    assert!(
        text.contains("direct chd_<redacted> chdt_<redacted> chdp_<redacted>"),
        "{text}"
    );
    for t in [TOKEN_MSG, TOKEN_FIELD, TOKEN_SPAN] {
        assert!(!text.contains(t), "{text}");
    }
}

/// A readiness `fail` line: the three secret forms redacted, newlines and control characters
/// spaces, one line, the message cut to 512 bytes.
#[test]
fn fail_line_is_one_redacted_line() {
    let message = format!(
        "a\nb\x07 {TOKEN_MSG} {TOKEN_FIELD} {TOKEN_SPAN} {}",
        "é".repeat(400)
    );
    let l = fail_line("store_open", Some(7373), &message);
    assert!(l.ends_with('\n') && l.matches('\n').count() == 1, "{l:?}");
    assert!(
        l.starts_with(
            "fail store_open port_saved=7373 a b  chd_<redacted> chdt_<redacted> chdp_<redacted> "
        ),
        "{l:?}"
    );
    for t in [TOKEN_MSG, TOKEN_FIELD, TOKEN_SPAN] {
        assert!(!l.contains(t), "{l}");
    }
    let m = l.trim_end().splitn(4, ' ').nth(3).unwrap();
    assert!(m.len() <= 512 && m.len() > 500, "{}", m.len());
}
