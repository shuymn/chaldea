//! Launches the installed Claude Code and Codex CLIs through `native()` (manual; ignored by
//! default): `cargo test -p chaldea-pty-runtime --test real_cli -- --ignored --nocapture`.
//!
//! Each CLI runs from its absolute path, with a minimal environment, in a fresh temp
//! directory, and receives no prompt. The test checks that the UI draws, that the CLI has
//! the PTY as controlling terminal in the foreground, that a resize makes it redraw, and
//! that the tree kill leaves no process of the tree. A trust or login screen counts as
//! drawn; the final screen is printed.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use chaldea_pty_runtime::{
    CloseBounds, Line, PtySize, Session, SessionConfig, SpawnSpec, native, with_spawn_lock,
};

const WAIT: Duration = Duration::from_secs(20);

fn run(cmd: &str, args: &[&str]) -> String {
    let out = with_spawn_lock(|| Command::new(cmd).args(args).output()).unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The canonical absolute path of `name` on the test's `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let p = run("/usr/bin/which", &[name]);
    let p = p.trim();
    (!p.is_empty()).then(|| PathBuf::from(p).canonicalize().ok())?
}

/// `(pid, ppid, pgid)` of every process.
fn table() -> Vec<(u32, u32, u32)> {
    run("/bin/ps", &["-axo", "pid=,ppid=,pgid="])
        .lines()
        .filter_map(|l| {
            let v: Vec<u32> = l
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            (v.len() == 3).then(|| (v[0], v[1], v[2]))
        })
        .collect()
}

/// The leader's group and every descendant of the leader.
fn tree(leader: u32) -> BTreeSet<u32> {
    let t = table();
    let mut set: BTreeSet<u32> = t.iter().filter(|p| p.2 == leader).map(|p| p.0).collect();
    set.insert(leader);
    loop {
        let more: Vec<u32> = t
            .iter()
            .filter(|p| set.contains(&p.1) && !set.contains(&p.0))
            .map(|p| p.0)
            .collect();
        if more.is_empty() {
            return set;
        }
        set.extend(more);
    }
}

fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn launch(name: &str) {
    let Some(bin) = which(name) else {
        panic!("{name} is not on PATH");
    };
    let dir = std::env::temp_dir().join(format!("chaldea-real-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    // The CLI's own directory, and node's for a `#!/usr/bin/env node` launcher.
    let mut path = OsString::new();
    for dir in [Some(bin.clone()), which("node")].into_iter().flatten() {
        path.push(dir.parent().unwrap());
        path.push(":");
    }
    path.push("/usr/bin:/bin");
    let mut env: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), path),
        ("TERM".into(), "xterm-256color".into()),
        ("LANG".into(), "en_US.UTF-8".into()),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".into(), home));
    }
    let spec = SpawnSpec {
        argv: vec![bin.clone().into()],
        cwd: dir.clone(),
        env,
        size: PtySize::new(100, 30).unwrap(),
    };
    let session = Session::spawn(&*native(), &spec, SessionConfig::new(|_| {})).unwrap();
    let offset = || {
        session
            .snapshot(Instant::now() + Duration::from_secs(1))
            .map_or(0, |s| s.output_offset)
    };
    assert!(wait_until(|| offset() > 0), "{name}: no output");
    // Let the first screen settle, then read the facts the CLI sees.
    std::thread::sleep(Duration::from_secs(3));
    let leader = session.identity().pid;
    let t: Vec<String> = tree(leader).iter().map(u32::to_string).collect();
    println!(
        "{name}: tree {}",
        run(
            "/bin/ps",
            &[
                "-o",
                "pid,ppid,pgid,tpgid,stat,tty,command",
                "-p",
                &t.join(",")
            ]
        )
    );
    let ps = run(
        "/bin/ps",
        &["-o", "pid=,pgid=,tpgid=,tty=", "-p", &leader.to_string()],
    );
    let f: Vec<&str> = ps.split_whitespace().collect();
    assert_eq!(f.len(), 4, "{ps}");
    assert_eq!(f[1], leader.to_string(), "{name}: not a group leader: {ps}");
    assert_eq!(
        f[2], f[1],
        "{name}: its group is not in the foreground: {ps}"
    );
    assert!(
        f[3].starts_with("ttys"),
        "{name}: no controlling terminal: {ps}"
    );
    let before = offset();
    let _resized = session.resize(PtySize::new(120, 40).unwrap()).unwrap();
    assert!(
        wait_until(|| offset() > before),
        "{name}: no redraw after resize"
    );
    std::thread::sleep(Duration::from_secs(1));
    let snap = session
        .snapshot(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert_eq!((snap.cols, snap.rows), (120, 40));
    println!("---- {name} ({}) screen ----", bin.display());
    for l in &snap.lines {
        let t = Line::text(l);
        if !t.trim().is_empty() {
            println!("{}", t.trim_end());
        }
    }
    let members = tree(leader);
    println!("{name}: tree before kill {members:?}");
    let report = session.kill(&[], Instant::now() + Duration::from_secs(5));
    println!("{name}: {report:?}");
    assert!(report.tree_dead, "{name}: {report:?}");
    let alive: BTreeSet<u32> = table().iter().map(|p| p.0).collect();
    let left: Vec<u32> = members
        .iter()
        .copied()
        .filter(|p| *p != leader && alive.contains(p))
        .collect();
    assert!(left.is_empty(), "{name}: survivors {left:?}");
    let out = session.close(&[], CloseBounds::default());
    assert!(out.released.tree_dead, "{out:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "launches the installed claude binary"]
fn claude_code_draws_resizes_and_dies() {
    launch("claude");
}

#[test]
#[ignore = "launches the installed codex binary"]
fn codex_draws_resizes_and_dies() {
    launch("codex");
}
