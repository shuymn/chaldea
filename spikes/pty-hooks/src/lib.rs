//! PTY / hooks spike harness. Design: `docs/spikes/pty-hooks-spike.md`.
//! OS-specific code lives only under `os/`.

pub mod cli;
pub mod emitter;
pub mod fake_cli;
pub mod http_client;
pub mod marks;
pub mod os;
pub mod paste;
pub mod pty;
pub mod receiver;
pub mod report;
pub mod scenario;
pub mod session;
pub mod util;
pub mod verdict;
pub mod vt;
