//! PR8-CL-9 (Unix): `SIGTERM`, `SIGINT`, and `SIGHUP` each stop a serving daemon with exit 0
//! and a free lock.
#![cfg(unix)]

mod support;

use chaldea_daemon::lifecycle::LockProbe;
use rustix::process::Signal;
use support::{Proc, lock_state, run_cmd, state, term};

#[test]
fn term_int_hup() {
    for sig in [Signal::TERM, Signal::INT, Signal::HUP] {
        let dir = state();
        let mut d = Proc::spawn(&mut run_cmd(&dir.path, None));
        d.serving();
        term(d.pid(), sig);
        assert_eq!(d.code(), 0, "{sig:?}: {}", d.stderr());
        assert_eq!(lock_state(&dir.path), LockProbe::Free, "{sig:?}");
        assert!(
            d.stderr().contains("exiting: code 0"),
            "{sig:?}: {}",
            d.stderr()
        );
    }
}
