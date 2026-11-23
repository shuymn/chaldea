//! Process setup, once per process (design §2.3): the signal state before any thread exists,
//! the panic hook, and the log subsystem.

use std::sync::Once;

use chaldea_pty_runtime::host;

use super::{log, panic};

/// Resets the inherited signal state, installs the panic hook, and starts the log queue and
/// the global subscriber. Call first in `main` (before argument parsing) and in every test
/// helper process; a second call does nothing.
pub fn prepare_process() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let signals = host::reset_process_signals();
        panic::install();
        log::install_process();
        if let Err(e) = signals
            && let Some(l) = log::process_logger()
        {
            l.warn(&format!(
                "the inherited signal state could not be reset: {e}"
            ));
        }
    });
}
