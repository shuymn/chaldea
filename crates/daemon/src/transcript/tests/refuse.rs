//! C1-9: a transcript path that is not an absolute regular file is never read, and the
//! poll returns at once: the store keeps serving. A missing file is read once it appears.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Fx, serving};
use crate::transcript::read::Step;

/// One poll of `fx`'s tailer: its step, after checking it returned at once, stored nothing,
/// and left the store serving.
fn poll_once(fx: &Fx) -> Step {
    let mut tail = fx.tail();
    let started = Instant::now();
    let step = tail.poll(&fx.backend());
    assert!(started.elapsed() < Duration::from_secs(5), "{step:?}");
    assert!(fx.events().is_empty(), "{:?}", fx.events());
    assert_eq!(fx.cursor(), None);
    serving(&fx.store);
    step
}

#[test]
fn a_relative_path_ends_the_tailer() {
    let fx = Fx::at(|_| PathBuf::from("relative/session.jsonl"));
    assert!(matches!(poll_once(&fx), Step::End(_)));
}

#[test]
fn a_directory_is_not_read() {
    let fx = Fx::at(|dir| dir.join("a-dir"));
    std::fs::create_dir(&fx.path).unwrap();
    assert_eq!(poll_once(&fx), Step::Wait);
}

/// A FIFO with no writer: a blocking open would wait forever.
#[cfg(target_os = "macos")]
#[test]
fn a_fifo_is_not_read() {
    use std::process::{Command, Stdio};
    let fx = Fx::at(|dir| dir.join("fifo.jsonl"));
    let mut cmd = Command::new("/usr/bin/mkfifo");
    cmd.arg(&fx.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = crate::lifecycle::spawn_locked(&mut cmd)
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success());
    assert_eq!(poll_once(&fx), Step::Wait);
    // Polled again, still nothing and still at once.
    assert_eq!(poll_once(&fx), Step::Wait);
}

/// The device and pipe namespaces are refused before any open.
#[cfg(windows)]
#[test]
fn a_device_namespace_path_ends_the_tailer() {
    for p in [r"\\.\pipe\chaldea-transcript-none", r"\\.\NUL"] {
        let fx = Fx::at(|_| PathBuf::from(p));
        assert!(matches!(poll_once(&fx), Step::End(_)), "{p}");
    }
}

/// A path that does not exist yet is retried, and read once the file appears.
#[test]
fn a_missing_file_is_read_once_it_appears() {
    let fx = Fx::new();
    assert_eq!(poll_once(&fx), Step::Wait);
    let mut tail = fx.tail();
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    fx.write(super::assistant("late").as_bytes());
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    assert_eq!(super::shorts(&fx.events()), ["generation:1", "text:late"]);
}
