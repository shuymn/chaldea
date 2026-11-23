//! The signal listeners (design §2.8): tasks on the runtime that call the gate, ended by the
//! process's exit. Unix `SIGTERM`, `SIGINT`, `SIGHUP`; Windows Ctrl-C and Ctrl-Break request a
//! shutdown, and a console close, logoff, or shutdown is a crash exit.

use std::io;
use std::sync::Arc;

use super::gate::{ShutdownGate, SignalName, StopCause};

/// Installs every listener; an error means none may be relied on (`Internal` at step 3).
#[cfg(unix)]
pub(crate) fn start(gate: &Arc<ShutdownGate>) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    for (kind, name) in [
        (SignalKind::terminate(), SignalName::Term),
        (SignalKind::interrupt(), SignalName::Int),
        (SignalKind::hangup(), SignalName::Hup),
    ] {
        let mut s = signal(kind)?;
        let gate = gate.clone();
        tokio::spawn(async move {
            while s.recv().await.is_some() {
                gate.request(StopCause::Signal(name));
            }
        });
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn start(gate: &Arc<ShutdownGate>) -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

    use super::gate::ConsoleEvent;

    let mut c = ctrl_c()?;
    let mut b = ctrl_break()?;
    let mut close = ctrl_close()?;
    let mut logoff = ctrl_logoff()?;
    let mut shutdown = ctrl_shutdown()?;
    let g = gate.clone();
    tokio::spawn(async move {
        while c.recv().await.is_some() {
            g.request(StopCause::Signal(SignalName::CtrlC));
        }
    });
    let g = gate.clone();
    tokio::spawn(async move {
        while b.recv().await.is_some() {
            g.request(StopCause::Signal(SignalName::CtrlBreak));
        }
    });
    let g = gate.clone();
    tokio::spawn(async move {
        if close.recv().await.is_some() {
            g.crash_exit(ConsoleEvent::Close);
        }
    });
    let g = gate.clone();
    tokio::spawn(async move {
        if logoff.recv().await.is_some() {
            g.crash_exit(ConsoleEvent::Logoff);
        }
    });
    let g = gate.clone();
    tokio::spawn(async move {
        if shutdown.recv().await.is_some() {
            g.crash_exit(ConsoleEvent::Shutdown);
        }
    });
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn start(_gate: &Arc<ShutdownGate>) -> io::Result<()> {
    Ok(())
}
