//! C1-10, tailer half: the final read after `launch_exited` (a trailing partial line, a
//! flaky read), a resume that continues the generation, and a resume reserved before the
//! final read.

use super::{Fx, TestBackend, assistant, shorts};
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

/// Makes the tailer's next read fail while held: on Unix the path cannot be opened (mode
/// 000), so the check before the commit fails with "permission denied"; on Windows another
/// handle holds an exclusive lock, so a read fails with a lock violation.
struct Flaky {
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    _locked: std::fs::File,
}

impl Flaky {
    fn new(fx: &Fx) -> Flaky {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fx.path, std::fs::Permissions::from_mode(0o000)).unwrap();
            Flaky {
                path: fx.path.clone(),
            }
        }
        #[cfg(windows)]
        {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fx.path)
                .unwrap();
            f.lock().unwrap();
            Flaky { _locked: f }
        }
    }
}

impl Drop for Flaky {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
    }
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
    let flaky = Flaky::new(&fx);
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
    let _flaky = Flaky::new(&fx);
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
fn a_resume_before_the_final_read_is_stale_binding() {
    let fx = Fx::new();
    fx.write(assistant("one").as_bytes());
    let mut first = fx.tail();
    fx.settle(&mut first);
    fx.append(assistant("two").as_bytes());
    fx.add(vec![support::exited(fx.t, fx.l)]);
    let l2 = support::launch(2);
    let be = TestBackend {
        store: &fx.store,
        after_read: Box::new(|| fx.add(vec![support::reserved(fx.t, l2)])),
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
