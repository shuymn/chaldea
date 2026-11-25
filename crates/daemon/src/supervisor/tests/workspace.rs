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
