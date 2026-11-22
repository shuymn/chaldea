//! Test fixtures of Chaldea (`docs/design/pty-unix.md` §2.8).
//!
//! [`fake_cli`] is `chaldea-fake-cli`, a stand-in for an agent CLI in real-PTY tests: each
//! of its processes connects a loopback TCP stream to the test ([`report`]), sends a hello
//! once its mechanism is in place, then mode events, and answers `ping` with `pong`. The
//! stream's EOF is the process's death, an oracle independent of the backend under test.

pub mod fake_cli;
pub mod report;
