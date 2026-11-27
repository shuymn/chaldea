//! `chaldea open [--print-url]` (docs/design/daemon-lifecycle.md §2.13, http-surface §8 D12):
//! a pair code minted with the owner token and no `Origin`, handed to the browser in the
//! pairing URL's fragment. The URL (and so the code) is printed or handed over, never logged.
//!
//! The interactive form first checks that the daemon serves the PWA (`GET /`, no Bearer;
//! docs/design/w1-pwa.md §2.1): a daemon built without it answers 501, and `open` then exits 1
//! before minting a code or launching anything. `--print-url` skips the check.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use chaldea_core::http::{ErrorBody, PairCodeResponse};
use chaldea_daemon::lifecycle::{ExitCode, diag};

use crate::client::{self, Discovery};

/// The deadline of each request (the page check, the pair-code request).
const REQUEST_WAIT: Duration = Duration::from_secs(5);

/// What `open` prints when the daemon answers `GET /` with 501.
pub(crate) const NO_PWA: &str =
    "this chaldea was built without the PWA; build it with the release command";

/// The port of the daemon serving `dir`.
fn port_of(dir: &Path) -> Result<u16, String> {
    match client::discover(dir) {
        Discovery::Serving(r) => r
            .port
            .ok_or_else(|| "the daemon's record names no port".to_owned()),
        _ => Err("daemon not running; run `chaldea daemon start`".to_owned()),
    }
}

/// The page check: `probe`'s status of `GET /` is 200, else the message to print.
fn check_page(probe: &dyn Fn() -> io::Result<u16>) -> Result<(), String> {
    match probe() {
        Ok(200) => Ok(()),
        Ok(501) => Err(NO_PWA.to_owned()),
        Ok(s) => Err(format!("the page check failed: {s}")),
        Err(e) => Err(format!("the page check failed: {e}")),
    }
}

/// The pairing URL of the daemon serving `dir` on `port`.
///
/// # Errors
///
/// The message to print (already free of secrets).
pub(crate) fn pair_url(dir: &Path, port: u16) -> Result<String, String> {
    let token = client::read_owner_token(dir)?;
    let r = client::request(
        port,
        "POST",
        "/api/pair-codes",
        Some(&token),
        Instant::now() + REQUEST_WAIT,
    )
    .map_err(|e| format!("the pair-code request failed: {e}"))?;
    if r.status != 200 {
        let code = serde_json::from_slice::<ErrorBody>(&r.body).map_or_else(
            |_| "an unreadable error".to_owned(),
            |b| {
                serde_json::to_value(b.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            },
        );
        return Err(format!(
            "the daemon refused the pair code: {} {code}",
            r.status
        ));
    }
    let code = serde_json::from_slice::<PairCodeResponse>(&r.body)
        .map_err(|e| format!("the pair-code response is not readable: {e}"))?
        .code;
    Ok(format!("http://127.0.0.1:{port}/#pair={code}"))
}

/// Hands `url` to `launcher`, or prints it with `print_url` or when the launcher fails.
pub(crate) fn deliver(
    url: &str,
    print_url: bool,
    launcher: &dyn Fn(&str) -> io::Result<()>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if print_url {
        let _ = writeln!(out, "{url}");
        return ExitCode::Ok;
    }
    if let Err(e) = launcher(url) {
        let _ = writeln!(
            err,
            "chaldea: {}",
            diag(&format!(
                "could not open a browser ({e}); open this URL yourself"
            ))
        );
        let _ = writeln!(out, "{url}");
    }
    ExitCode::Ok
}

/// `chaldea open`: the URL of the daemon serving `dir`, handed to `launcher` (the binary
/// passes `host::open_url`, its test entry a recorder).
pub(crate) fn run(
    dir: &Path,
    print_url: bool,
    launcher: &dyn Fn(&str) -> io::Result<()>,
) -> ExitCode {
    let (mut out, mut err) = (io::stdout(), io::stderr());
    let port = match port_of(dir) {
        Ok(p) => p,
        Err(m) => {
            let _ = writeln!(err, "chaldea: {}", diag(&m));
            return ExitCode::Failed;
        }
    };
    let probe = || client::request_status(port, "GET", "/", None, Instant::now() + REQUEST_WAIT);
    let mint = || pair_url(dir, port);
    open_with(print_url, &probe, &mint, launcher, &mut out, &mut err)
}

/// [`run`] after discovery: the page check (`probe`) unless `print_url`, then the pairing URL
/// (`mint`), delivered.
fn open_with(
    print_url: bool,
    probe: &dyn Fn() -> io::Result<u16>,
    mint: &dyn Fn() -> Result<String, String>,
    launcher: &dyn Fn(&str) -> io::Result<()>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let url = if print_url {
        mint()
    } else {
        check_page(probe).and_then(|()| mint())
    };
    match url {
        Ok(url) => deliver(&url, print_url, launcher, out, err),
        Err(m) => {
            let _ = writeln!(err, "chaldea: {}", diag(&m));
            ExitCode::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;

    use super::{NO_PWA, deliver, open_with};

    const URL: &str =
        "http://127.0.0.1:7373/#pair=chdp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    /// PR8-CL-24: the launcher receives the exact URL, once, and nothing is printed.
    #[test]
    fn hands_the_url_to_the_launcher() {
        let seen = RefCell::new(Vec::new());
        let launcher = |u: &str| -> io::Result<()> {
            seen.borrow_mut().push(u.to_owned());
            Ok(())
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = deliver(URL, false, &launcher, &mut out, &mut err);
        assert_eq!(code.code(), 0);
        assert_eq!(*seen.borrow(), vec![URL.to_owned()]);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    /// PR8-CL-24: a failing launcher: the URL on stdout with the notice, exit 0.
    #[test]
    fn launcher_failure_prints_the_url() {
        let launcher = |_: &str| -> io::Result<()> { Err(io::Error::other("no browser")) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = deliver(URL, false, &launcher, &mut out, &mut err);
        assert_eq!(code.code(), 0);
        assert_eq!(String::from_utf8(out).unwrap(), format!("{URL}\n"));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("open this URL yourself"), "{err}");
    }

    /// `--print-url` prints and never launches.
    #[test]
    fn print_url_does_not_launch() {
        let launcher = |_: &str| -> io::Result<()> { panic!("launched") };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = deliver(URL, true, &launcher, &mut out, &mut err);
        assert_eq!(code.code(), 0);
        assert_eq!(String::from_utf8(out).unwrap(), format!("{URL}\n"));
    }

    /// What one `open_with` run did: its exit code, stdout, stderr, the launcher's URLs, and
    /// how often the probe and the mint ran.
    struct Run {
        code: i32,
        out: String,
        err: String,
        launched: Vec<String>,
        probes: usize,
        mints: usize,
    }

    fn open(print_url: bool, status: io::Result<u16>) -> Run {
        let status = RefCell::new(Some(status));
        let (probes, mints, launched) = (RefCell::new(0), RefCell::new(0), RefCell::new(vec![]));
        let probe = || {
            *probes.borrow_mut() += 1;
            status.borrow_mut().take().expect("one probe")
        };
        let mint = || {
            *mints.borrow_mut() += 1;
            Ok(URL.to_owned())
        };
        let launcher = |u: &str| -> io::Result<()> {
            launched.borrow_mut().push(u.to_owned());
            Ok(())
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = open_with(print_url, &probe, &mint, &launcher, &mut out, &mut err);
        Run {
            code: code.code(),
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            launched: launched.take(),
            probes: probes.take(),
            mints: mints.take(),
        }
    }

    /// W1-9 `open_without_pwa`: a 501 page check exits 1 with the message, before any code is
    /// minted or anything launched.
    #[test]
    fn open_without_pwa() {
        let r = open(false, Ok(501));
        assert_eq!(r.code, 1);
        assert!(r.err.contains(NO_PWA), "{}", r.err);
        assert_eq!((r.probes, r.mints), (1, 0));
        assert!(r.launched.is_empty() && r.out.is_empty(), "{}", r.out);
    }

    /// A 200 page check goes on to the mint and the launcher.
    #[test]
    fn open_with_pwa_launches() {
        let r = open(false, Ok(200));
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!((r.probes, r.mints), (1, 1));
        assert_eq!(r.launched, vec![URL.to_owned()]);
    }

    /// Any other status or a transport error fails as a refused pair code does: exit 1, no
    /// code, no launch.
    #[test]
    fn failed_page_check_is_an_error() {
        for status in [
            Ok(404),
            Ok(503),
            Err(io::Error::from(io::ErrorKind::TimedOut)),
        ] {
            let r = open(false, status);
            assert_eq!(r.code, 1);
            assert!(r.err.contains("the page check failed"), "{}", r.err);
            assert!(!r.err.contains(NO_PWA), "{}", r.err);
            assert_eq!(r.mints, 0);
            assert!(r.launched.is_empty());
        }
    }

    /// `--print-url` skips the page check.
    #[test]
    fn print_url_skips_the_page_check() {
        let r = open(true, Ok(501));
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!((r.probes, r.mints), (0, 1));
        assert_eq!(r.out, format!("{URL}\n"));
        assert!(r.launched.is_empty());
    }
}
