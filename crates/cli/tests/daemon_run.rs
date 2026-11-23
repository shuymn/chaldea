//! `chaldea daemon run` as a binary (design §2.2, §2.5–§2.7, §2.13): PR8-CL-3 (the lock across
//! processes), CL-6 (crash receipts), CL-20 (redacted usage and startup errors), CL-21 (the
//! owner token file), CL-22 (ports), CL-23 (state directory resolution).

mod support;

use std::ffi::OsStr;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chaldea_core::command::ReceiptStatus;
use chaldea_core::ids::{CommandId, TokenId};
use chaldea_daemon::http::verify_secret_in;
use chaldea_daemon::lifecycle::{LockProbe, STATE_DIR_ENV, names};
use chaldea_daemon::store::TokenSelector;
use chaldea_pty_runtime::host::{self, EntryKind, Privacy};
use support::{
    Proc, chaldea, close_store, free_port, get_status, listing, lock_state, marked_dir, open_store,
    run_cmd, run_to_exit, serving_record, state, token_of, wait_for, write_private,
};

/// The design's port range (§2.6), restated.
const FIRST: u16 = 7373;
const LAST: u16 = 7472;

/// Tests that let the daemon choose from the range run one at a time; every other test passes
/// `--port` with an ephemeral port.
static RANGE: Mutex<()> = Mutex::new(());

fn range() -> MutexGuard<'static, ()> {
    RANGE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bindable(p: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, p)).is_ok()
}

/// The first port of the range nothing holds right now.
fn first_free_in_range() -> u16 {
    (FIRST..=LAST)
        .find(|&p| bindable(p))
        .expect("a free port in the range")
}

/// Starts a daemon on `dir` with `--port <ephemeral>` and waits for it to serve.
fn serve(dir: &Path) -> (Proc, u16) {
    let p = Proc::spawn(&mut run_cmd(dir, Some(free_port())));
    let port = p.serving();
    (p, port)
}

/// Starts, serves, and stops a daemon on `dir` (a state directory with a database, a token, and
/// a saved port).
fn serve_once(dir: &Path, port: Option<u16>) -> u16 {
    let mut p = Proc::spawn(&mut run_cmd(dir, Some(port.unwrap_or_else(free_port))));
    let got = p.serving();
    assert_eq!(p.stop_and_wait(dir), 0, "{}", p.stderr());
    got
}

fn saved_port(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(names::PORT)).unwrap()
}

// ---- PR8-CL-3 ----

#[test]
fn second_run_exits_3_and_touches_nothing() {
    let dir = state();
    let (mut a, port) = serve(&dir.path);
    let before = listing(&dir.path);
    let (code, _out, err) = run_to_exit(&mut run_cmd(&dir.path, Some(free_port())));
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("already holds"), "{err}");
    assert!(err.contains(&format!("pid {}", a.pid())), "{err}");
    assert!(err.contains(&format!("http://127.0.0.1:{port}")), "{err}");
    assert_eq!(listing(&dir.path), before);
    assert_eq!(a.stop_and_wait(&dir.path), 0);
}

#[test]
fn reacquire_after_kill() {
    let dir = state();
    let (mut a, _) = serve(&dir.path);
    a.child.kill().unwrap();
    assert!(a.wait().code() != Some(0));
    // Within the lock's 2 s retry the next run either succeeds or reports the holder.
    let mut b = Proc::spawn(&mut run_cmd(&dir.path, Some(free_port())));
    let deadline = Instant::now() + support::WAIT;
    let b_serves = loop {
        if let Some(s) = b.child.try_wait().unwrap() {
            assert_eq!(s.code(), Some(3), "{}", b.stderr());
            assert!(b.stderr().contains("already holds"), "{}", b.stderr());
            break false;
        }
        if serving_pid(&dir.path) == Some(b.pid()) {
            break true;
        }
        assert!(Instant::now() < deadline, "{}", b.stderr());
        std::thread::sleep(Duration::from_millis(20));
    };
    if !b_serves {
        assert!(wait_for(|| lock_state(&dir.path) == LockProbe::Free));
        b = Proc::spawn(&mut run_cmd(&dir.path, Some(free_port())));
    }
    b.serving();
    assert_eq!(b.stop_and_wait(&dir.path), 0);
}

fn serving_pid(dir: &Path) -> Option<u32> {
    match chaldea_daemon::lifecycle::discover(dir) {
        chaldea_daemon::lifecycle::Discovery::Serving(r) => Some(r.pid),
        _ => None,
    }
}

// ---- PR8-CL-6 ----

#[test]
fn in_progress_receipt_becomes_unknown() {
    let dir = state();
    serve_once(&dir.path, None);
    let cmd = CommandId::from_u128(0x5000_0000_0000_0000_0000_0000_0000_0001);
    let token = TokenId::from_u128(0x6000_0000_0000_0000_0000_0000_0000_0001);
    let store = open_store(&dir.path);
    support::settle(
        store
            .submit(Instant::now() + support::WAIT, move |tx| {
                Ok(tx.receipt_begin(cmd, token, [7; 32])?)
            })
            .unwrap(),
    );
    close_store(&store);

    serve_once(&dir.path, None);
    let store = open_store(&dir.path);
    let status = store
        .read(Instant::now() + support::WAIT, |tx| tx.receipt(cmd))
        .unwrap()
        .unwrap()
        .status;
    close_store(&store);
    assert_eq!(status, ReceiptStatus::UnknownAfterCrash);
}

// ---- PR8-CL-20 ----

#[test]
fn usage_error_is_redacted() {
    let secret = format!("chd_{}", "Q".repeat(43));
    let (code, out, err) = run_to_exit(&mut chaldea(&["daemon", "run", "--port", &secret], &[]));
    assert_eq!(code, 2, "{err}");
    assert!(!err.is_empty());
    assert!(!err.contains(&secret), "{err}");
    assert!(!out.contains(&secret), "{out}");
}

/// A refused state directory whose path holds a token: the refusal names it redacted.
#[test]
fn startup_failure_output_is_redacted() {
    let tmp = support::temp_parent();
    let secret = format!("chd_{}", "R".repeat(43));
    let dir = tmp.path().join(&secret);
    host::create_private_dir(&dir).unwrap();
    std::fs::write(dir.join("unrelated"), b"x").unwrap();
    let (code, out, err) = run_to_exit(&mut run_cmd(&dir, Some(free_port())));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not a Chaldea state directory"), "{err}");
    assert!(!err.contains(&secret), "{err}");
    assert!(!out.contains(&secret), "{out}");
}

// ---- PR8-CL-21 ----

#[test]
fn issues_owner_token() {
    let dir = state();
    let (mut d, port) = serve(&dir.path);
    let path = dir.path.join(names::OWNER_TOKEN);
    assert_eq!(
        host::privacy(&path, EntryKind::File).unwrap(),
        Privacy::Private
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    #[cfg(windows)]
    {
        let (owner, mut allows) = chaldea_pty_runtime::fake::descriptor_sids(&path).unwrap();
        let me = chaldea_pty_runtime::fake::user_sid().unwrap();
        assert_eq!(owner, me);
        allows.sort();
        let mut want = vec![me, "S-1-5-18".to_owned()];
        want.sort();
        assert_eq!(allows, want);
    }
    let token = token_of(&dir.path);
    assert_eq!(get_status(port, "/api/threads", Some(&token)), 200);
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

#[test]
fn keeps_a_valid_token_across_restart() {
    let dir = state();
    serve_once(&dir.path, None);
    let t1 = token_of(&dir.path);
    let (mut d, port) = serve(&dir.path);
    assert_eq!(token_of(&dir.path), t1);
    assert_eq!(get_status(port, "/api/threads", Some(&t1)), 200);
    assert!(
        !d.stderr().contains("issued a new owner token"),
        "{}",
        d.stderr()
    );
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

#[test]
fn reissues_a_revoked_token() {
    let dir = state();
    serve_once(&dir.path, None);
    let t1 = token_of(&dir.path);
    let store = open_store(&dir.path);
    let secret = t1.clone();
    let id = store
        .read(Instant::now() + support::WAIT, move |tx| {
            Ok(verify_secret_in(tx, &secret).map(|a| a.token_id))
        })
        .unwrap()
        .unwrap();
    let revoked = support::settle(
        store
            .submit(Instant::now() + support::WAIT, move |tx| {
                Ok(tx.token_revoke(TokenSelector::Token(id))?)
            })
            .unwrap(),
    );
    assert_eq!(revoked, 1);
    close_store(&store);

    let (mut d, port) = serve(&dir.path);
    let t2 = token_of(&dir.path);
    assert_ne!(t2, t1);
    assert!(
        d.stderr().contains("issued a new owner token"),
        "{}",
        d.stderr()
    );
    assert_eq!(get_status(port, "/api/threads", Some(&t1)), 401);
    assert_eq!(get_status(port, "/api/threads", Some(&t2)), 200);
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

/// Refused before the log or the store is touched: no database, no `daemon.log`, the file
/// unchanged.
fn assert_refused_untouched(dir: &Path, token_bytes: &[u8], want: &str) {
    let (code, _out, err) = run_to_exit(&mut run_cmd(dir, Some(free_port())));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(want), "{err}");
    for f in names::DB_FILES {
        assert!(!dir.join(f).exists(), "{f} was created");
    }
    assert!(!dir.join(names::LOG).exists());
    assert_eq!(
        std::fs::read(dir.join(names::OWNER_TOKEN)).unwrap(),
        token_bytes
    );
    assert!(!dir.join(names::RECORD).exists() || lock_state(dir) == LockProbe::Free);
}

#[test]
fn refuses_a_foreign_token_file() {
    let dir = state();
    marked_dir(&dir.path);
    let bytes = b"not a token\n";
    write_private(&dir.path.join(names::OWNER_TOKEN), bytes);
    assert_refused_untouched(&dir.path, bytes, "not a Chaldea token file");
}

#[test]
fn refuses_a_valid_token_with_wrong_permissions() {
    // A token a daemon issued, copied into a fresh state directory without a database.
    let issued = state();
    serve_once(&issued.path, None);
    let bytes = std::fs::read(issued.path.join(names::OWNER_TOKEN)).unwrap();
    let dir = state();
    marked_dir(&dir.path);
    let path = dir.path.join(names::OWNER_TOKEN);
    write_private(&path, &bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    #[cfg(windows)]
    chaldea_pty_runtime::fake::grant_users_read(&path).unwrap();
    assert_refused_untouched(
        &dir.path,
        &bytes,
        "Removing the file does not revoke the token: revoke it",
    );
}

// ---- PR8-CL-22 ----

#[test]
fn first_port_from_the_range() {
    let _range = range();
    let dir = state();
    let expected = first_free_in_range();
    let mut d = Proc::spawn(&mut run_cmd(&dir.path, None));
    let port = d.serving();
    assert_eq!(port, expected);
    assert_eq!(saved_port(&dir.path), format!("{port}\n"));
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

#[test]
fn taken_first_port_is_skipped() {
    let _range = range();
    let dir = state();
    let held_port = first_free_in_range();
    let _held = TcpListener::bind((Ipv4Addr::LOCALHOST, held_port)).unwrap();
    // The next free port after the held one, not any later one.
    let expected = (held_port + 1..=LAST)
        .find(|&p| bindable(p))
        .expect("a second free port in the range");
    let mut d = Proc::spawn(&mut run_cmd(&dir.path, None));
    let port = d.serving();
    assert_eq!(port, expected);
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

#[test]
fn same_port_after_restart() {
    let dir = state();
    let first = serve_once(&dir.path, None);
    let mut d = Proc::spawn(&mut run_cmd(&dir.path, None));
    assert_eq!(d.serving(), first);
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

#[test]
fn failed_override_keeps_the_saved_port() {
    let dir = state();
    let saved = serve_once(&dir.path, None);
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let q = taken.local_addr().unwrap().port();
    let (code, _out, err) = run_to_exit(&mut run_cmd(&dir.path, Some(q)));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("port {q} is unavailable")), "{err}");
    assert_eq!(saved_port(&dir.path), format!("{saved}\n"));
}

#[test]
fn taken_saved_port_is_reported() {
    let dir = state();
    let saved = serve_once(&dir.path, None);
    let _taken = TcpListener::bind((Ipv4Addr::LOCALHOST, saved)).unwrap();
    let os_error = TcpListener::bind((Ipv4Addr::LOCALHOST, saved))
        .unwrap_err()
        .to_string();
    let (code, _out, err) = run_to_exit(&mut run_cmd(&dir.path, None));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(&format!("port {saved} (saved for")), "{err}");
    assert!(err.contains(&os_error), "{err}\nwant: {os_error}");
    assert!(err.contains("chaldea open"), "{err}");
    assert_eq!(saved_port(&dir.path), format!("{saved}\n"));
}

#[test]
fn malformed_port_file_fails_before_the_store() {
    let dir = state();
    marked_dir(&dir.path);
    write_private(&dir.path.join(names::PORT), b"seventy\n");
    let (code, _out, err) = run_to_exit(&mut run_cmd(&dir.path, None));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("is not a port number"), "{err}");
    for f in names::DB_FILES {
        assert!(!dir.path.join(f).exists(), "{f} was created");
    }
    assert_eq!(
        std::fs::read(dir.path.join(names::PORT)).unwrap(),
        b"seventy\n"
    );
}

/// A non-loopback address of this host (the source address a route to a documentation
/// address would use; nothing is sent), if there is one.
fn non_loopback_address() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn not_reachable_on_other_addresses() {
    let dir = state();
    let (mut d, port) = serve(&dir.path);
    let r = serving_record(&dir.path);
    assert_eq!(
        r.url.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(
        s.peer_addr().unwrap(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    );
    drop(s);
    if let Some(ip) = non_loopback_address() {
        let r = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2));
        assert!(r.is_err(), "reachable on {ip}");
    }
    assert_eq!(d.stop_and_wait(&dir.path), 0);
}

// ---- PR8-CL-23 ----

#[test]
fn flag_overrides_env() {
    let (a, b) = (state(), state());
    let mut d = Proc::spawn(
        chaldea(&[], &[(STATE_DIR_ENV, b.path.as_os_str())])
            .arg("--state-dir")
            .arg(&a.path)
            .args(["daemon", "run", "--port", &free_port().to_string()]),
    );
    d.serving();
    assert_eq!(serving_record(&a.path).pid, d.pid());
    assert!(!b.path.exists());
    assert_eq!(d.stop_and_wait(&a.path), 0);
}

#[test]
fn env_only_selects_the_dir() {
    let b = state();
    let mut d = Proc::spawn(&mut chaldea(
        &["daemon", "run", "--port", &free_port().to_string()],
        &[(STATE_DIR_ENV, OsStr::new(&b.path))],
    ));
    d.serving();
    assert_eq!(serving_record(&b.path).pid, d.pid());
    assert_eq!(d.stop_and_wait(&b.path), 0);
}

/// `--help` goes to stdout, with nothing on stderr, and exits 0.
#[test]
fn help_goes_to_stdout() {
    let (code, out, err) = run_to_exit(&mut chaldea(&["--help"], &[]));
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Usage"), "{out}");
    assert!(err.is_empty(), "{err}");
}

/// PR8-CL-1: `--state-dir <link>/` (a trailing slash) is refused like `<link>`: the target
/// stays empty.
#[cfg(target_os = "macos")]
#[test]
fn refuses_a_final_symlink_with_a_trailing_slash() {
    let root = support::temp_parent();
    let real = root.path().join("real");
    host::create_private_dir(&real).unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut spelling = link.into_os_string();
    spelling.push("/");
    let (code, _out, err) = run_to_exit(&mut run_cmd(Path::new(&spelling), Some(free_port())));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("link"), "{err}");
    assert!(std::fs::read_dir(&real).unwrap().next().is_none());
}

/// A FIFO at `daemon.json`: the start is refused within the shutdown budget, the lock is
/// free, the FIFO untouched, and no database created.
#[cfg(target_os = "macos")]
#[test]
fn fifo_record_refusal_releases_the_lock() {
    use std::os::unix::fs::FileTypeExt;
    use std::process::{Command, Stdio};
    let dir = state();
    marked_dir(&dir.path);
    let record = dir.path.join(names::RECORD);
    let (code, _, err) = run_to_exit(
        Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(&record)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    assert_eq!(code, 0, "{err}");
    let started = Instant::now();
    let (code, _, err) = run_to_exit(&mut run_cmd(&dir.path, Some(free_port())));
    assert_eq!(code, 1, "{err}");
    assert!(started.elapsed() < Duration::from_secs(11), "{err}");
    assert_eq!(lock_state(&dir.path), LockProbe::Free);
    assert!(
        std::fs::symlink_metadata(&record)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert!(!dir.path.join(names::DB).exists());
}

/// After a clean stop and after a kill, the next start without `--port` serves the same port
/// with the same owner token, and an authenticated request succeeds on both sides.
#[test]
fn restarts_keep_a_usable_saved_endpoint() {
    for killed in [false, true] {
        let dir = state();
        let (mut first, port) = serve(&dir.path);
        let before = serving_record(&dir.path);
        let token = token_of(&dir.path);
        assert_eq!(saved_port(&dir.path), format!("{port}\n"));
        assert_eq!(
            support::request_complete(port, "GET", "/api/threads", &token, None).0,
            200
        );
        if killed {
            support::kill_and_reap(&mut first.child);
            let stale = chaldea_daemon::lifecycle::read_record(&dir.path)
                .unwrap()
                .unwrap();
            assert_eq!(stale.instance, before.instance);
        } else {
            assert_eq!(first.stop_and_wait(&dir.path), 0, "{}", first.stderr());
            assert!(!dir.path.join(names::RECORD).exists());
        }
        drop(first);
        assert!(wait_for(|| lock_state(&dir.path) == LockProbe::Free));
        let mut next = Proc::spawn(&mut run_cmd(&dir.path, None));
        assert_eq!(next.serving(), port, "killed={killed}");
        let after = serving_record(&dir.path);
        assert_eq!(after.pid, next.pid());
        assert_ne!(after.instance, before.instance);
        assert_eq!(
            (after.port, after.url.as_deref()),
            (Some(port), before.url.as_deref())
        );
        assert_eq!(saved_port(&dir.path), format!("{port}\n"));
        assert_eq!(token_of(&dir.path), token);
        assert_eq!(
            support::request_complete(port, "GET", "/api/threads", &token, None).0,
            200
        );
        assert_eq!(next.stop_and_wait(&dir.path), 0, "{}", next.stderr());
    }
}

/// The daemon serves the MVP command table: an MVP command no PR has implemented yet answers
/// 501 `not_implemented` and leaves no receipt.
#[test]
fn mvp_command_is_501_without_receipt() {
    let dir = state();
    let (mut d, port) = serve(&dir.path);
    let token = token_of(&dir.path);
    let cmd = CommandId::from_u128(0x5000_0000_0000_0000_0000_0000_0000_00c1);
    let (status, body) = support::request_complete(
        port,
        "POST",
        "/api/commands/spawn",
        &token,
        Some(&format!(r#"{{"command_id":"{cmd}","args":{{}}}}"#)),
    );
    assert_eq!(status, 501, "{body}");
    assert!(body.contains("not_implemented"), "{body}");
    assert_eq!(d.stop_and_wait(&dir.path), 0, "{}", d.stderr());
    let store = open_store(&dir.path);
    let receipt = store
        .read(Instant::now() + support::WAIT, move |tx| tx.receipt(cmd))
        .unwrap();
    close_store(&store);
    assert!(receipt.is_none(), "{receipt:?}");
}
