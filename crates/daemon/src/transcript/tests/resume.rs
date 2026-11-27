//! C1-10, tailer half: the final read after `launch_exited` (a trailing partial line, a
//! flaky read), a resume that continues the generation, and a resume reserved before or
//! during the final read.

use super::{Flaky, Fx, TestBackend, assistant, shorts};
use crate::store::tests::support;
use crate::transcript::read::{FINAL_TRIES, Step};

/// After the exit, the final read stores the rest; trailing bytes without a newline are
/// `partial_line`, and the cursor ends after them.
#[test]
fn exit_with_a_trailing_partial_line() {
    let fx = Fx::new();
    let body = [assistant("one").as_str(), "{\"type\":\"assi"].concat();
    fx.write(body.as_bytes());
    let mut tail = fx.tail();
    assert_eq!(fx.settle(&mut tail), Step::Wait);
    fx.add(vec![support::exited(fx.t, fx.l)]);
    assert!(matches!(fx.settle(&mut tail), Step::End(_)));
    let evs = fx.events();
    assert_eq!(
        shorts(&evs),
        [
            "generation:1",
            "text:one",
            "raw:partial_line:{\"type\":\"assi"
        ]
    );
    assert_eq!(evs[2].payload["content_type"], "application/octet-stream");
    assert_eq!(fx.cursor().unwrap().read_offset, body.len() as u64);
}

/// A final read that fails is retried at the next poll; once it succeeds the rest is read.
/// One that fails `FINAL_TRIES` polls in a row ends the tailer, the bytes left in the file.
#[test]
fn a_flaky_final_read_is_retried_then_ended() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let flaky = Flaky::new(&fx.path);
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    drop(flaky);
    assert!(matches!(fx.settle(&mut tail), Step::End(_)));
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );

    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let _flaky = Flaky::new(&fx.path);
    for _ in 1..FINAL_TRIES {
        assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    }
    let end = tail.poll(&fx.backend());
    assert!(
        matches!(&end, Step::End(why) if why.contains("final read failed")),
        "{end:?}"
    );
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
}

/// A resume of the same session binds the next launch to the same path: its tailer
/// verifies the prefix and continues generation 1 from the cursor.
#[test]
fn a_resume_continues_the_same_generation() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    let l2 = support::launch(2);
    let path = fx.path.to_string_lossy().into_owned();
    fx.add(vec![
        support::exited(fx.t, fx.l),
        support::reserved(fx.t, l2),
        support::started(fx.t, l2),
        support::bound(fx.t, l2, "s1", Some(&path)),
    ]);
    assert!(matches!(first.poll(&fx.backend()), Step::End(_)));
    fx.append(assistant("two").as_bytes());
    let mut next = fx.tail_of(l2);
    assert_eq!(fx.settle(&mut next), Step::Wait);
    let evs = fx.events();
    assert_eq!(shorts(&evs), ["generation:1", "text:one", "text:two"]);
    assert_eq!(evs[2].generation, Some(1));
}

/// A resume reserved after the old tailer's final read was read and before it commits:
/// `StaleBinding` ends it with nothing stored, and the next launch's tailer reads those
/// bytes from the cursor.
#[test]
fn a_resume_reserved_after_the_final_read_is_stale_binding() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let l2 = support::launch(2);
    let be = TestBackend {
        after_read: Box::new(|| fx.add(vec![support::reserved(fx.t, l2)])),
        ..fx.backend()
    };
    let end = first.poll(&be);
    assert!(
        matches!(&end, Step::End(why) if why.contains("StaleBinding")),
        "{end:?}"
    );
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
    let path = fx.path.to_string_lossy().into_owned();
    fx.add(vec![
        support::started(fx.t, l2),
        support::bound(fx.t, l2, "s1", Some(&path)),
    ]);
    let mut next = fx.tail_of(l2);
    fx.settle(&mut next);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
}

/// A resume reserved before the old tailer's final read: the poll ends it with nothing
/// read or stored, and the next launch's tailer reads those bytes from the cursor.
#[test]
fn a_resume_reserved_before_the_final_read_ends_it() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    fx.append(assistant("two").as_bytes());
    let l2 = support::launch(2);
    fx.add(vec![
        support::exited(fx.t, fx.l),
        support::reserved(fx.t, l2),
    ]);
    let end = first.poll(&fx.backend());
    assert!(
        matches!(&end, Step::End(why) if why.contains("no longer the bound one")),
        "{end:?}"
    );
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
    let path = fx.path.to_string_lossy().into_owned();
    fx.add(vec![
        support::started(fx.t, l2),
        support::bound(fx.t, l2, "s1", Some(&path)),
    ]);
    let mut next = fx.tail_of(l2);
    assert_eq!(fx.settle(&mut next), Step::Wait);
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
}

/// A poll whose view fails says nothing about the exit (the driver retries a final read
/// then); the next view finds the exit and the final read stores the rest.
#[test]
fn a_failed_view_leaves_the_exit_unknown() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    assert_eq!(tail.exited(), Some(false));
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let be = TestBackend {
        fail_views: std::cell::Cell::new(1),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::Wait);
    assert_eq!(tail.exited(), None);
    assert!(matches!(fx.settle(&mut tail), Step::End(_)));
    assert_eq!(tail.exited(), Some(true));
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
}

/// A final read that opens the file (a new tailer) and stops during the prefix check at
/// that open: the continuity is recorded as unverified.
#[test]
fn a_stop_during_the_final_reads_open_check_is_unverified() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let mut next = fx.tail_with(|_| {}, std::sync::Arc::default());
    next.stop_at_prefix_check();
    assert_eq!(next.poll(&fx.backend()), Step::End("stopped".into()));
    assert!(!next.prefix_checked());
    assert_eq!(next.stats.unverified, 1);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
}

/// A final read that never opens the file (it was never created) ends after `FINAL_TRIES`
/// polls in the normal course of a launch's exit, with the continuity recorded as
/// unverified.
#[test]
fn a_final_read_that_never_opens_is_unverified() {
    let fx = Fx::new();
    let mut tail = fx.tail();
    assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    fx.add(vec![support::exited(fx.t, fx.l)]);
    for _ in 1..FINAL_TRIES {
        assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    }
    assert!(matches!(tail.poll(&fx.backend()), Step::End(_)));
    assert_eq!(tail.stats.unverified, 1);
}

/// The final read checked the prefix, then the check after its commit could not open the
/// path, so the handle is dropped and its check with it: when the path does not open again
/// before the retries are spent, the continuity is recorded as unverified. (On Windows the
/// held lock does not stop the open, so the check after the commit passes.)
#[cfg(unix)]
#[test]
fn a_dropped_handle_needs_the_final_prefix_check_again() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let flaky = std::cell::RefCell::new(None);
    let be = TestBackend {
        before_commit: Box::new(|_| *flaky.borrow_mut() = Some(Flaky::new(&fx.path))),
        ..fx.backend()
    };
    assert_eq!(tail.poll(&be), Step::Again);
    for _ in 1..FINAL_TRIES {
        assert_eq!(tail.poll(&fx.backend()), Step::Wait);
    }
    assert!(matches!(tail.poll(&fx.backend()), Step::End(_)));
    assert_eq!(tail.stats.unverified, 1);
    drop(flaky.borrow_mut().take());
}

/// The final read reads half a line at the end it saw at its start, and Claude completes the
/// line before the check: the bytes are not committed as `partial_line`, and the next batch
/// stores the whole line with the cursor at its end.
#[test]
fn a_line_completed_during_the_final_read_is_stored_whole() {
    let fx = Fx::new();
    let one = assistant("one");
    fx.write(one.as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    let two = assistant("two");
    let (head, rest) = two.split_at(two.len() / 2);
    fx.append(head.as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let done = std::cell::Cell::new(false);
    let be = TestBackend {
        after_read: Box::new(|| {
            if !done.replace(true) {
                fx.append(rest.as_bytes());
            }
        }),
        ..fx.backend()
    };
    assert!(matches!(fx.settle_with(&mut tail, &be), Step::End(_)));
    assert_eq!(
        shorts(&fx.events()),
        ["generation:1", "text:one", "text:two"]
    );
    assert_eq!(
        fx.cursor().unwrap().read_offset,
        (one.len() + two.len()) as u64
    );
}

/// A line Claude keeps extending through the final read: the `FINAL_TRIES`-th read that finds
/// it still growing ends the tailer, the cursor stays before the line, and the line is
/// recorded as unread.
#[test]
fn a_line_growing_through_the_final_read_ends_it_unread() {
    let fx = Fx::new();
    let one = assistant("one");
    fx.write(one.as_bytes());
    let mut tail = fx.tail();
    fx.settle(&mut tail);
    fx.append(b"{\"type\":\"assistant\"");
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let reads = std::cell::Cell::new(0u32);
    let be = TestBackend {
        after_read: Box::new(|| {
            reads.set(reads.get() + 1);
            fx.append(b" ");
        }),
        ..fx.backend()
    };
    for _ in 1..FINAL_TRIES {
        assert_eq!(tail.poll(&be), Step::Again);
    }
    assert!(matches!(tail.poll(&be), Step::End(_)));
    assert_eq!(reads.get(), FINAL_TRIES);
    assert_eq!(shorts(&fx.events()), ["generation:1", "text:one"]);
    assert_eq!(fx.cursor().unwrap().read_offset, one.len() as u64);
    assert_eq!(tail.stats.unread, 1);
}
