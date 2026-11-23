//! Process setup, once per process (design §2.3): the signal state before any thread exists,
//! the panic hook, and the log subsystem.

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Once, OnceLock};

use chaldea_pty_runtime::host;

use super::{log, panic};

/// Resets the inherited signal state, installs the panic hook, and starts the log queue and
/// the global subscriber. Call first in the CLI's `dispatch` (after its `hook-emit` branch,
/// before argument parsing) and in every test helper process; a second call does nothing.
pub fn prepare_process() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[cfg(test)]
        SETUP_RUNS.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        let signals = if FAIL_SIGNAL_RESET.load(Ordering::SeqCst) {
            Err(std::io::Error::other("injected reset failure"))
        } else {
            host::reset_process_signals()
        };
        #[cfg(not(test))]
        let signals = host::reset_process_signals();
        panic::install();
        log::install_process();
        if let Err(e) = signals {
            let _ = SETUP_ERROR.set(format!(
                "the inherited signal state could not be reset: {e}"
            ));
        }
    });
}

static SETUP_ERROR: OnceLock<String> = OnceLock::new();

/// Why the process setup failed, if it did: `run` refuses to start (design §2.3).
pub(crate) fn setup_error() -> Option<&'static str> {
    SETUP_ERROR.get().map(String::as_str)
}

#[cfg(test)]
static SETUP_RUNS: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static FAIL_SIGNAL_RESET: AtomicBool = AtomicBool::new(false);

/// How often the setup body ran in this process (tests).
#[cfg(test)]
pub(crate) fn setup_runs() -> u32 {
    SETUP_RUNS.load(Ordering::SeqCst)
}

/// Makes the next setup's signal reset fail (tests; call before `prepare_process`).
#[cfg(test)]
pub(crate) fn fail_signal_reset_for_test() {
    FAIL_SIGNAL_RESET.store(true, Ordering::SeqCst);
}
