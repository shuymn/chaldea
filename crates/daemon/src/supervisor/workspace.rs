//! The workspace probe (design §2.3 step 1): a thread's `workspace_id` from git's common
//! directory, so a repository and its worktrees share it, else from the canonical cwd. At
//! most one git child exists; the [`GitLease`] is never waited for.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::ids::WorkspaceId;
use tokio::sync::Notify;

use super::tracker::TrackedChild;
use super::{GIT_PROBE, Pause, Supervisor, lock};

/// The poll period of a git child.
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
struct SlotState {
    busy: bool,
    left: Option<TrackedChild>,
}

/// The one git child's slot: a `busy` flag and at most one `left` child awaiting its reap.
#[derive(Debug, Default)]
pub(crate) struct GitSlot {
    state: Mutex<SlotState>,
    /// Woken when the lease is dropped (a child may have moved into `left`).
    pub(crate) changed: Notify,
}

impl GitSlot {
    /// The lease, only when the slot is not busy and holds no child.
    pub(crate) fn lease(self: &Arc<Self>) -> Option<GitLease> {
        let mut st = lock(&self.state);
        if st.busy || st.left.is_some() {
            return None;
        }
        st.busy = true;
        Some(GitLease {
            slot: self.clone(),
            child: None,
        })
    }

    /// Kills and reaps the `left` child, if any.
    pub(crate) fn reap_left(&self) {
        let mut st = lock(&self.state);
        if let Some(c) = st.left.as_mut() {
            c.kill();
            if c.try_reap().is_some() {
                st.left = None;
            }
        }
    }

    /// The lease free and `left` empty.
    pub(crate) fn idle(&self) -> bool {
        let st = lock(&self.state);
        !st.busy && st.left.is_none()
    }
}

/// The right to run one git child. Its drop, on every path including a panic, kills an
/// unreaped child through its own handle, moves it with its token into `left`, clears
/// `busy`, and wakes [`GitSlot::changed`].
#[derive(Debug)]
pub(crate) struct GitLease {
    slot: Arc<GitSlot>,
    child: Option<TrackedChild>,
}

impl Drop for GitLease {
    fn drop(&mut self) {
        let mut st = lock(&self.slot.state);
        if let Some(mut c) = self.child.take()
            && !c.reaped()
        {
            c.kill();
            st.left = Some(c);
        }
        st.busy = false;
        drop(st);
        self.slot.changed.notify_waiters();
    }
}

/// Git's own variables (`git rev-parse --local-env-vars`), each able to point discovery at
/// another repository or change what git reads there, and `GIT_DISCOVERY_ACROSS_FILESYSTEM`,
/// which lets discovery cross into a parent repository on another filesystem: the probe's
/// `git` runs without them and discovers from `cwd` by git's defaults.
/// `GIT_CEILING_DIRECTORIES` stays, since it can only stop discovery, making the canonical
/// cwd the id.
const GIT_LOCAL_ENV: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` without git's own
/// variables ([`GIT_LOCAL_ENV`]), bounded by `deadline` (supervisor clock). `Err` names why
/// the canonical cwd is used instead.
pub(crate) fn common_dir(
    sup: &Supervisor,
    cwd: &Path,
    deadline: Instant,
) -> Result<PathBuf, &'static str> {
    let Some(mut lease) = sup.git.lease() else {
        return Err("the git slot is busy or holds a child");
    };
    let Some((program, lead)) = sup.cfg.git.split_first() else {
        return Err("no git program");
    };
    let mut cmd = Command::new(program);
    cmd.args(lead)
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    #[cfg(test)]
    if let Some(env) = &sup.cfg.git_env {
        cmd.env_clear().envs(env.iter().cloned());
    }
    for name in GIT_LOCAL_ENV {
        cmd.env_remove(name);
    }
    chaldea_pty_runtime::host::no_console_window(&mut cmd);
    let real_deadline = sup.real(deadline);
    let child = match TrackedChild::spawn(&mut cmd, &sup.tracker, real_deadline, sup.pauses()) {
        None => return Err("git was not created (spawn lock, admission, or deadline)"),
        Some(Err(_)) => return Err("git could not be spawned"),
        Some(Ok(c)) => c,
    };
    let c = lease.child.insert(child);
    sup.pauses().blocking(Pause::GitAfterCreate);
    let mut stdout = c.take_stdout();
    loop {
        if !sup.tracker.accepting() {
            c.kill();
        }
        match chaldea_pty_runtime::host::observed_before(real_deadline, || c.try_reap()) {
            Some(Some(status)) => {
                if !status.success() {
                    return Err("git exited nonzero");
                }
                let mut out = String::new();
                if let Some(s) = stdout.as_mut() {
                    let _ = s.read_to_string(&mut out);
                }
                // Only the line end git adds (LF; CRLF on Windows): a path may itself hold
                // spaces, newlines, or on Unix a trailing CR.
                let path = out.strip_suffix('\n').unwrap_or(&out);
                #[cfg(windows)]
                let path = path.strip_suffix('\r').unwrap_or(path);
                if path.is_empty() {
                    return Err("git printed no path");
                }
                return Ok(PathBuf::from(path));
            }
            Some(None) => std::thread::sleep(POLL),
            None => {
                c.kill();
                return Err("git did not exit by its deadline");
            }
        }
    }
}

/// The thread's workspace id: from git's common directory when the probe succeeds, else
/// from the canonical cwd, with the reason logged.
pub(crate) async fn workspace_of(sup: &Arc<Supervisor>, cwd: &Path) -> WorkspaceId {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let deadline = sup.now() + GIT_PROBE;
    let s = sup.clone();
    let dir = canonical.clone();
    let task = sup
        .rt
        .spawn_blocking(move || common_dir(&s, &dir, deadline));
    let wait = deadline + Duration::from_secs(1);
    let path = match crate::deadline::settle_before(|| sup.now(), wait, sup.timer(wait), task)
        .await
        .in_time()
    {
        Some(Ok(Ok(p))) => std::fs::canonicalize(&p).unwrap_or(p),
        Some(Ok(Err(why))) => {
            tracing::info!(cwd = %canonical.display(), reason = why, "workspace id from the cwd");
            canonical
        }
        _ => {
            tracing::info!(cwd = %canonical.display(), "workspace id from the cwd: the probe did not return");
            canonical
        }
    };
    chaldea_core::launch::workspace_id(&path.to_string_lossy())
}
