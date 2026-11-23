//! macOS binary tests (`harness = false`, support/runner.rs; nothing runs on another OS).
//!
//! - PR8-CL-13: a detached daemon is in its own session; on a GUI host it was launched through
//!   its own launchd job (its label gone once `start` returned); a failed GUI-domain probe, a
//!   refused submission, or an unresolved one fails `start` instead of falling back to
//!   `setsid`; a timed-out `bootstrap` is resolved by its label within the reserve; only stale
//!   labels are removed. Every launchd job carries the test's own label prefix.
//! - PR8-CL-17: a process that starts with `SIGCHLD` ignored and `SIGTERM`, `SIGINT`, `SIGHUP`,
//!   `SIGCHLD` blocked gets them restored by `prepare_process`, so a child stays waitable by
//!   exact pid. std's spawn clears the child's signal mask (and only `posix_spawn`/`exec`
//!   without std would keep it), so the helper sets the inherited state on itself and then runs
//!   the same `prepare_process` call `dispatch` makes first; a control helper skips it and sees
//!   `ECHILD`.
//!
//! Literals are the design's: 15 s readiness deadline, 3 s GUI-domain probe and label reserve,
//! 60 s label staleness.

mod support;

fn main() {
    #[cfg(target_os = "macos")]
    support::runner::main(&macos::CASES);
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use chaldea_daemon::lifecycle::{Discovery, discover, prepare_process, spawn_locked};
    use chaldea_pty_runtime::fake;
    use chaldea_pty_runtime::host::{self, GuiDomain, SigchldDisposition, UnixSignal};
    use std::os::unix::process::CommandExt;

    use super::support::faults::{self, Labels};
    use super::support::runner::Case;
    use super::support::{Proc, get_status, report, run_helper, state, token_of};

    pub(super) const CASES: [Case; 10] = [
        Case {
            name: "inherited_signal_state_is_reset",
            run: inherited_signal_state_is_reset,
        },
        Case {
            name: "detached_child_has_its_own_session",
            run: detached_child_has_its_own_session,
        },
        Case {
            name: "gui_host_uses_launchd",
            run: gui_host_uses_launchd,
        },
        Case {
            name: "failed_domain_probe_fails_the_start",
            run: failed_domain_probe_fails_the_start,
        },
        Case {
            name: "refused_submission_fails_the_start",
            run: refused_submission_fails_the_start,
        },
        Case {
            name: "timed_out_bootstrap_is_resolved_by_label",
            run: timed_out_bootstrap_is_resolved_by_label,
        },
        Case {
            name: "timeout_then_absent_then_present",
            run: timeout_then_absent_then_present,
        },
        Case {
            name: "completed_bootstrap_failure_is_final",
            run: completed_bootstrap_failure_is_final,
        },
        Case {
            name: "indeterminate_submission_times_out",
            run: indeterminate_submission_times_out,
        },
        Case {
            name: "stale_labels_only",
            run: stale_labels_only,
        },
    ];

    const READY_TIMEOUT: Duration = Duration::from_secs(15);
    const PROBE_SHARE: Duration = Duration::from_secs(3);

    // ---- PR8-CL-17 ----

    const MODE: &str = "CHALDEA_CL17_MODE";

    fn true_child() -> std::io::Result<std::process::ExitStatus> {
        let mut c = spawn_locked(
            Command::new("/usr/bin/true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )?;
        c.wait()
    }

    fn in_helper(mode: &str) {
        fake::ignore_sigchld().unwrap();
        fake::block_signals(&UnixSignal::ALL).unwrap();
        let before = host::signal_state().unwrap();
        assert_eq!(before.sigchld, SigchldDisposition::Ignore);
        assert_eq!(before.blocked, UnixSignal::ALL.to_vec());
        if mode == "reset" {
            prepare_process();
            let after = host::signal_state().unwrap();
            assert_eq!(after.sigchld, SigchldDisposition::Default, "{after:?}");
            assert!(!after.no_cldwait, "{after:?}");
            assert!(after.blocked.is_empty(), "{after:?}");
            let status = true_child().unwrap();
            assert!(status.success(), "{status:?}");
            report("waited");
        } else {
            let e = true_child().unwrap_err();
            assert_eq!(
                e.raw_os_error(),
                Some(rustix::io::Errno::CHILD.raw_os_error()),
                "{e}"
            );
            report("echild");
        }
    }

    fn inherited_signal_state_is_reset() {
        if let Ok(mode) = std::env::var(MODE) {
            in_helper(&mode);
            return;
        }
        for (mode, want) in [("reset", "waited"), ("control", "echild")] {
            let got = run_helper(
                "inherited_signal_state_is_reset",
                &[(MODE, OsStr::new(mode))],
                Duration::from_secs(30),
                |_| {},
            );
            assert_eq!(got, want, "{mode}");
        }
    }

    // ---- PR8-CL-13 ----

    fn start_cmd(labels: &Labels, dir: &Path, env: &[(&str, &OsStr)]) -> Command {
        let mut c = faults::chaldea(labels, &[OsStr::new("--state-dir"), dir.as_os_str()]);
        c.args(["daemon", "start"]);
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn stop(labels: &Labels, dir: &Path) {
        let mut c = faults::chaldea(labels, &[OsStr::new("--state-dir"), dir.as_os_str()]);
        c.args(["daemon", "stop"]);
        let mut p = Proc::spawn(&mut c);
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
    }

    struct StopOnDrop<'a>(&'a Labels, &'a Path);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            if !matches!(discover(self.1), Discovery::NotRunning) {
                let mut c =
                    faults::chaldea(self.0, &[OsStr::new("--state-dir"), self.1.as_os_str()]);
                c.args(["daemon", "stop"]);
                let _ = Proc::spawn(&mut c).wait_within(Duration::from_secs(30));
            }
        }
    }

    fn sid(pid: u32) -> i32 {
        let p = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        rustix::process::getsid(Some(p))
            .unwrap()
            .as_raw_nonzero()
            .get()
    }

    /// The real GUI-domain probe, printed so the log shows which branch a runner has.
    fn gui_domain() -> GuiDomain {
        let g = host::gui_domain(Instant::now() + PROBE_SHARE);
        eprintln!("gui domain of this host: {g:?}");
        g
    }

    /// The launchd cases submit real jobs: they need the user's GUI domain.
    fn require_gui() {
        let g = gui_domain();
        assert_eq!(
            g,
            GuiDomain::Available,
            "this case submits a launchd job and needs the user's GUI domain"
        );
    }

    fn label_of(stderr: &str) -> String {
        stderr
            .lines()
            .find_map(|l| l.split("detaching through launchd as ").nth(1))
            .map(|l| l.trim().to_owned())
            .unwrap_or_else(|| panic!("no launchd label in {stderr}"))
    }

    /// No daemon serves `dir` and nothing holds it.
    fn assert_no_daemon(dir: &Path) {
        assert_eq!(discover(dir), Discovery::NotRunning);
    }

    /// PR8-CL-13: the `setsid` path (forced by `SSH_CONNECTION`, the deliberate SSH path): the
    /// daemon leads its own session; the starter's process group, killed after the start, does
    /// not take the daemon with it.
    fn detached_child_has_its_own_session() {
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let mut c = start_cmd(
            &labels,
            &d.path,
            &[("SSH_CONNECTION", OsStr::new("10.0.0.1 1 10.0.0.2 22"))],
        );
        c.process_group(0);
        let mut p = Proc::spawn(&mut c);
        let starter = p.pid();
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
        assert!(
            p.stderr().contains("detaching through setsid"),
            "{}",
            p.stderr()
        );
        let Discovery::Serving(r) = discover(&d.path) else {
            panic!("not serving")
        };
        assert_eq!(r.detached.as_deref(), Some("setsid"));
        assert_eq!(sid(r.pid), i32::try_from(r.pid).unwrap());
        let group = rustix::process::Pid::from_raw(i32::try_from(starter).unwrap()).unwrap();
        // The starter has exited; anything still in its group is killed.
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        std::thread::sleep(Duration::from_millis(200));
        assert!(matches!(discover(&d.path), Discovery::Serving(_)));
        assert_eq!(
            get_status(r.port.unwrap(), "/api/threads", Some(&token_of(&d.path))),
            200
        );
        stop(&labels, &d.path);
    }

    /// PR8-CL-13: on a GUI host the daemon was launched through its own launchd job, leads its
    /// own session, and the starter removed the job's label once the verdict arrived. Without a
    /// GUI domain the `setsid` path ran instead, asserted with the reason printed.
    fn gui_host_uses_launchd() {
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let g = gui_domain();
        let mut p = Proc::spawn(&mut start_cmd(&labels, &d.path, &[]));
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
        let err = p.stderr();
        let Discovery::Serving(r) = discover(&d.path) else {
            panic!("not serving")
        };
        assert_eq!(sid(r.pid), i32::try_from(r.pid).unwrap());
        match g {
            GuiDomain::Available => {
                eprintln!("gui_host_uses_launchd: launchd branch");
                assert_eq!(r.detached.as_deref(), Some("launchd"));
                let label = label_of(&err);
                assert!(
                    err.contains(&format!("removed the launchd job {label}")),
                    "{err}"
                );
                assert!(
                    !labels.present().contains(&label),
                    "{label} is still loaded"
                );
            }
            GuiDomain::Absent { why } => {
                eprintln!("gui_host_uses_launchd: setsid branch ({why})");
                assert_eq!(r.detached.as_deref(), Some("setsid"));
            }
            GuiDomain::ProbeFailed { why } => panic!("the probe failed: {why}"),
        }
        stop(&labels, &d.path);
    }

    /// PR8-CL-13: the probe times out: exit 1 with the `daemon run` suggestion, within the
    /// probe's 3 s share, and no daemon (no `setsid` child either).
    fn failed_domain_probe_fails_the_start() {
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let t = Instant::now();
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(faults::LAUNCHCTL, OsStr::new("probe-timeout"))],
        ));
        assert_eq!(p.code(), 1, "{}{}", p.stdout(), p.stderr());
        let took = t.elapsed();
        let err = p.stderr();
        assert!(
            err.contains("could not determine whether a GUI session exists"),
            "{err}"
        );
        assert!(err.contains("chaldea daemon run"), "{err}");
        assert!(!err.contains("detaching through"), "{err}");
        assert!(
            took >= PROBE_SHARE && took < PROBE_SHARE + Duration::from_secs(2),
            "{took:?}"
        );
        assert_no_daemon(&d.path);
        assert!(!d.path.exists(), "a child touched the state directory");
    }

    /// PR8-CL-13: `bootstrap` refused, the label absent: exit 1, no daemon.
    fn refused_submission_fails_the_start() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(faults::LAUNCHCTL, OsStr::new("bootstrap-refused"))],
        ));
        assert_eq!(p.code(), 1, "{}{}", p.stdout(), p.stderr());
        let err = p.stderr();
        assert!(
            err.contains("launchd did not accept the daemon's job"),
            "{err}"
        );
        assert!(err.contains("chaldea daemon run"), "{err}");
        assert!(!err.contains("setsid"), "{err}");
        std::thread::sleep(Duration::from_millis(500));
        assert_no_daemon(&d.path);
        assert!(labels.present().is_empty());
    }

    /// PR8-CL-13: a non-zero `bootstrap` exit is final: one lookup, absent, exit 1 at once
    /// (well within the 3 s reserve).
    fn completed_bootstrap_failure_is_final() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let t = Instant::now();
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(faults::LAUNCHCTL, OsStr::new("bootstrap-refused"))],
        ));
        assert_eq!(p.code(), 1, "{}{}", p.stdout(), p.stderr());
        let lookups = p.stderr().matches("fault: print gui/").count();
        assert_eq!(lookups, 1, "{}", p.stderr());
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// PR8-CL-13: `bootstrap` registers the job but outlives its share: the reserved 3 s
    /// resolve the label, the start proceeds to a verdict, one daemon serves.
    fn timed_out_bootstrap_is_resolved_by_label() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let t = Instant::now();
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(faults::LAUNCHCTL, OsStr::new("bootstrap-late"))],
        ));
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
        // The share ends 3 s before the deadline.
        assert!(
            t.elapsed() >= READY_TIMEOUT - Duration::from_secs(3),
            "{:?}",
            t.elapsed()
        );
        assert!(p.stderr().contains("-> Some(0)"), "{}", p.stderr());
        let Discovery::Serving(r) = discover(&d.path) else {
            panic!("not serving")
        };
        assert_eq!(r.detached.as_deref(), Some("launchd"));
        stop(&labels, &d.path);
    }

    /// PR8-CL-13: `bootstrap` times out and the job registers only 1 s into the reserve: the
    /// first lookups answer absent, a later one present, and the start proceeds to a verdict
    /// instead of exiting 1.
    fn timeout_then_absent_then_present() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(
                faults::LAUNCHCTL,
                OsStr::new("bootstrap-timeout-register-later"),
            )],
        ));
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
        let err = p.stderr();
        let lookups: Vec<&str> = err
            .lines()
            .filter(|l| l.contains("fault: print gui/"))
            .collect();
        assert!(lookups.len() >= 2, "{err}");
        assert!(lookups[0].ends_with("Some(113)"), "{err}");
        assert!(lookups.last().unwrap().ends_with("Some(0)"), "{err}");
        assert!(matches!(discover(&d.path), Discovery::Serving(_)));
        stop(&labels, &d.path);
    }

    /// PR8-CL-13: `bootstrap` and every label lookup time out: exit 4 naming the label and its
    /// `bootout`, no `setsid` fallback.
    fn indeterminate_submission_times_out() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let t = Instant::now();
        let mut p = Proc::spawn(&mut start_cmd(
            &labels,
            &d.path,
            &[(faults::LAUNCHCTL, OsStr::new("all-timeout"))],
        ));
        assert_eq!(p.code(), 4, "{}{}", p.stdout(), p.stderr());
        let err = p.stderr();
        let label = label_of(&err);
        assert!(err.contains("launchctl bootout gui/"), "{err}");
        assert!(err.contains(&format!("{label}`")), "{err}");
        assert!(err.contains("may still be pending"), "{err}");
        assert!(!err.contains("setsid"), "{err}");
        assert!(t.elapsed() >= READY_TIMEOUT, "{:?}", t.elapsed());
        assert_no_daemon(&d.path);
    }

    /// Submits a one-shot job running `/usr/bin/true` under `label` and waits until it ran.
    fn submit_finished_job(label: &str, dir: &Path) {
        let plist = dir.join(format!("{label}.plist"));
        std::fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>/usr/bin/true</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><false/></dict></plist>\n"
            ),
        )
        .unwrap();
        let target = host::gui_domain_target();
        let o = host::launchctl(
            &[
                OsStr::new("bootstrap"),
                OsStr::new(&target),
                plist.as_os_str(),
            ],
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(o.code, Some(0), "{o:?}");
        let service = format!("{target}/{label}");
        let ran = super::support::wait_for(|| {
            host::launchctl(
                &[OsStr::new("print"), OsStr::new(&service)],
                Instant::now() + Duration::from_secs(10),
            )
            .is_ok_and(|o| o.code == Some(0) && !o.stdout.contains("state = running"))
        });
        assert!(ran, "{label} did not finish");
    }

    /// PR8-CL-13: before submitting, a start removes this prefix's labels proven stale (not
    /// running, older than 60 s) and keeps a recent one.
    fn stale_labels_only() {
        require_gui();
        let labels = Labels::new();
        let d = state();
        let _stop = StopOnDrop(&labels, &d.path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let old = format!(
            "{}{}-{}",
            labels.prefix,
            now - 120_000,
            uuid::Uuid::new_v4()
        );
        let recent = format!("{}{}-{}", labels.prefix, now - 5_000, uuid::Uuid::new_v4());
        let plists = tempfile::tempdir().unwrap();
        submit_finished_job(&old, plists.path());
        submit_finished_job(&recent, plists.path());
        let mut p = Proc::spawn(&mut start_cmd(&labels, &d.path, &[]));
        assert_eq!(p.code(), 0, "{}{}", p.stdout(), p.stderr());
        let present = labels.present();
        assert!(!present.contains(&old), "{present:?}");
        assert!(present.contains(&recent), "{present:?}");
        assert!(
            p.stderr()
                .contains(&format!("removed the stale launchd job {old}")),
            "{}",
            p.stderr()
        );
        stop(&labels, &d.path);
    }
}
