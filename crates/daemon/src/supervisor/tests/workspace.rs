//! The workspace probe (design §2.3 step 1; PR10a1-CL-28).

use std::path::Path;
use std::process::{Command, Stdio};

use super::Rig;
use crate::supervisor::workspace::workspace_of;

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = crate::lifecycle::spawn_locked(&mut cmd)
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository and its worktree share one id (git's common directory), also reached
/// through a symlink; a plain directory gets its own, from its canonical path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ids() {
    let rig = Rig::new().await;
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    let wt = root.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "w"],
    );
    let plain = root.path().join("plain");
    std::fs::create_dir(&plain).unwrap();

    let id_repo = workspace_of(&rig.sup, &repo).await;
    let id_wt = workspace_of(&rig.sup, &wt).await;
    assert_eq!(id_repo, id_wt, "a repository and its worktree");
    let id_plain = workspace_of(&rig.sup, &plain).await;
    assert_ne!(id_plain, id_repo);
    let canonical = std::fs::canonicalize(&plain).unwrap();
    assert_eq!(
        id_plain,
        chaldea_core::launch::workspace_id(&canonical.to_string_lossy())
    );
    #[cfg(unix)]
    {
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert_eq!(
            workspace_of(&rig.sup, &link).await,
            id_repo,
            "through a symlink"
        );
    }
    rig.srv.stop().await;
}

/// A `git` that ignores its arguments and sleeps far beyond every bound of these tests.
pub(super) fn sleeping_git() -> Vec<std::ffi::OsString> {
    let argv: &[&str] = if cfg!(windows) {
        &["powershell", "-NoProfile", "-Command", "Start-Sleep 30; #"]
    } else {
        &["sh", "-c", "exec sleep 30", "sh"]
    };
    argv.iter().map(std::ffi::OsString::from).collect()
}

fn canonical_id(dir: &Path) -> chaldea_core::ids::WorkspaceId {
    chaldea_core::launch::workspace_id(&std::fs::canonicalize(dir).unwrap().to_string_lossy())
}

/// PR10a1-CL-19 `probe_bounded`: a git that outlives the probe's 5 s: the canonical cwd at
/// the deadline, the child killed and left with its token in `left`; a probe meanwhile
/// uses the canonical cwd at once; a tick reaps the child, and the slot is free again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_bounded() {
    use std::time::{Duration, Instant};
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.git = sleeping_git();
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    assert_eq!(
        workspace_of(&rig.sup, dir.path()).await,
        canonical_id(dir.path())
    );
    let took = started.elapsed();
    assert!(took >= crate::supervisor::GIT_PROBE, "{took:?}");
    assert!(
        took < crate::supervisor::GIT_PROBE + Duration::from_secs(2),
        "{took:?}"
    );
    assert_eq!(rig.sup.tracker.count(), 1, "the left child keeps its token");
    assert!(!rig.sup.git.idle());
    // Meanwhile: no second git, the canonical cwd at once.
    let started = Instant::now();
    assert_eq!(
        workspace_of(&rig.sup, dir.path()).await,
        canonical_id(dir.path())
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(rig.sup.tracker.count(), 1);
    // The tick reaps it.
    rig.until("the tick reaped the child", || {
        futures_util::FutureExt::now_or_never(rig.sup.tick(rig.sup.now()));
        rig.sup.git.idle()
    })
    .await;
    assert_eq!(rig.sup.tracker.count(), 0);
    assert!(rig.sup.git.lease().is_some(), "a probe may run git again");
    rig.srv.stop().await;
}

/// PR10a1-CL-19 `admission_at_spawn`: admission closed while a probe holds the spawn lock
/// before its admit: no token, no git, the canonical cwd. A probe held after its admit
/// until past its deadline: no git, the token released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_at_spawn() {
    use crate::supervisor::Pause;
    use crate::supervisor::workspace::common_dir;
    use std::time::{Duration, Instant};
    const NOT_CREATED: &str = "git was not created (spawn lock, admission, or deadline)";
    let dir = tempfile::tempdir().unwrap();
    // Admission closed before the admit.
    let rig = Rig::new().await;
    let held = rig.pauses.arm(Pause::GitBeforeAdmit);
    let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
    let probe =
        std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(5)));
    held.wait();
    rig.sup.tracker.close_admission();
    held.go();
    assert_eq!(probe.join().unwrap(), Err(NOT_CREATED));
    assert_eq!(rig.sup.tracker.count(), 0);
    rig.srv.stop().await;
    // Held after the admit until past the deadline.
    let rig = Rig::new().await;
    let held = rig.pauses.arm(Pause::GitAfterAdmit);
    let deadline = rig.sup.now() + Duration::from_millis(200);
    let real = rig.sup.real(deadline);
    let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
    let probe = std::thread::spawn(move || common_dir(&sup, &cwd, deadline));
    held.wait();
    assert_eq!(rig.sup.tracker.count(), 1, "admitted");
    rig.until("past the probe's deadline", || Instant::now() > real)
        .await;
    held.go();
    assert_eq!(probe.join().unwrap(), Err(NOT_CREATED));
    assert_eq!(rig.sup.tracker.count(), 0, "the token released");
    assert!(rig.sup.git.idle());
    rig.srv.stop().await;
}

/// PR10a1-CL-19 `probe_panic`: a panic right after a sleeping git was created: the lease's
/// drop kills it and leaves it with its token in `left`, the lease free; the tick reaps it,
/// and a later graceful stop is `Clean`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_panic() {
    use crate::lifecycle::ParticipantEnd;
    use crate::supervisor::Pause;
    use crate::supervisor::workspace::common_dir;
    use std::time::Duration;
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.git = sleeping_git();
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let fault = rig.pauses.arm_panic(Pause::GitAfterCreate);
    let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
    let probe =
        std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(5)));
    fault.wait();
    fault.go();
    assert!(probe.join().is_err(), "the probe panicked");
    assert_eq!(
        rig.sup.tracker.count(),
        1,
        "the child in `left` keeps its token"
    );
    assert!(!rig.sup.git.idle());
    rig.until("the tick reaped the child", || {
        futures_util::FutureExt::now_or_never(rig.sup.tick(rig.sup.now()));
        rig.sup.git.idle()
    })
    .await;
    assert_eq!(rig.sup.tracker.count(), 0);
    let end = rig.sup.now() + Duration::from_secs(1);
    assert_eq!(
        crate::supervisor::components::shutdown(rig.sup.clone(), end).await,
        ParticipantEnd::Clean
    );
    rig.srv.stop().await;
}
