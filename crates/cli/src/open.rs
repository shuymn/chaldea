//! `chaldea open [--print-url]` (docs/design/daemon-lifecycle.md §2.13, http-surface §8 D12):
//! a pair code minted with the owner token and no `Origin`, handed to the browser in the
//! pairing URL's fragment. The URL (and so the code) is printed or handed over, never logged.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use chaldea_core::http::{ErrorBody, PairCodeResponse};
use chaldea_daemon::lifecycle::{ExitCode, diag};

use crate::client::{self, Discovery};

/// The pair-code request's deadline.
const REQUEST_WAIT: Duration = Duration::from_secs(5);

/// The pairing URL of the daemon serving `dir`.
///
/// # Errors
///
/// The message to print (already free of secrets).
pub(crate) fn pair_url(dir: &Path) -> Result<String, String> {
    let port = match client::discover(dir) {
        Discovery::Serving(r) => r
            .port
            .ok_or_else(|| "the daemon's record names no port".to_owned())?,
        _ => return Err("daemon not running; run `chaldea daemon start`".to_owned()),
    };
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
/// passes `host::open_url`).
pub(crate) fn run(
    dir: &Path,
    print_url: bool,
    launcher: &dyn Fn(&str) -> io::Result<()>,
) -> ExitCode {
    match pair_url(dir) {
        Ok(url) => deliver(
            &url,
            print_url,
            launcher,
            &mut io::stdout(),
            &mut io::stderr(),
        ),
        Err(m) => {
            let _ = writeln!(io::stderr(), "chaldea: {}", diag(&m));
            ExitCode::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;

    use super::deliver;

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
}
