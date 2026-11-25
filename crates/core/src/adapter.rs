//! The adapter capability table (`docs/mvp-design.md` §2.7; `docs/design/input-encoders.md`
//! §2.2): one row per `(Agent, HostOs)`, chosen at run time, never by DECSET 2004 or `cfg`.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::Agent;
use crate::payload::InputEncoding;

/// The OS of the host a daemon runs on. The daemon maps `std::env::consts::OS` to it once at
/// startup with [`HostOs::from_os_str`]; `core` never reads the host OS itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostOs {
    Macos,
    Windows,
}

impl HostOs {
    /// The host OS named by a `std::env::consts::OS` value: `"macos"` and `"windows"`;
    /// any other value is `None` (a daemon there refuses to start).
    #[must_use]
    pub fn from_os_str(os: &str) -> Option<HostOs> {
        match os {
            "macos" => Some(HostOs::Macos),
            "windows" => Some(HostOs::Windows),
            _ => None,
        }
    }
}

/// One row of `docs/mvp-design.md` §2.7, the columns the input encoders own.
///
/// Rows exist only in [`CAPABILITIES`]; the fields are private, so no caller outside `core`
/// can build a row or change a copy of one:
///
/// ```compile_fail,E0451
/// use std::time::Duration;
/// use chaldea_core::adapter::{Capabilities, HostOs};
/// use chaldea_core::ids::Agent;
/// use chaldea_core::payload::InputEncoding;
///
/// let _ = Capabilities {
///     agent: Agent::Codex,
///     os: HostOs::Macos,
///     pty_encoding: InputEncoding::Positional,
///     first_prompt: InputEncoding::Positional,
///     submit_delay: Duration::ZERO,
/// };
/// ```
///
/// ```compile_fail,E0616
/// use chaldea_core::adapter::{capabilities, HostOs};
/// use chaldea_core::ids::Agent;
/// use chaldea_core::payload::InputEncoding;
///
/// let mut r = *capabilities(Agent::Codex, HostOs::Macos);
/// r.pty_encoding = InputEncoding::Positional;
/// ```
///
/// ```compile_fail,E0624
/// use std::time::Duration;
/// use chaldea_core::adapter::{Capabilities, HostOs};
/// use chaldea_core::ids::Agent;
/// use chaldea_core::payload::InputEncoding;
///
/// let _ = Capabilities::new(
///     Agent::Codex,
///     HostOs::Macos,
///     InputEncoding::Positional,
///     InputEncoding::Positional,
///     Duration::ZERO,
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    agent: Agent,
    os: HostOs,
    pty_encoding: InputEncoding,
    first_prompt: InputEncoding,
    submit_delay: Duration,
}

/// The wait between the text part and the submit part of a send unit, on every row.
const SUBMIT_DELAY: Duration = Duration::from_millis(300);

impl Capabilities {
    /// A row with the given columns: used for [`CAPABILITIES`] and, in this crate's tests, for
    /// rows outside the table.
    pub(crate) const fn new(
        agent: Agent,
        os: HostOs,
        pty_encoding: InputEncoding,
        first_prompt: InputEncoding,
        submit_delay: Duration,
    ) -> Self {
        Capabilities {
            agent,
            os,
            pty_encoding,
            first_prompt,
            submit_delay,
        }
    }

    #[must_use]
    pub fn agent(&self) -> Agent {
        self.agent
    }

    #[must_use]
    pub fn os(&self) -> HostOs {
        self.os
    }

    /// How a prompt written to the PTY is encoded: `BracketedPaste` or `WinKeys`.
    #[must_use]
    pub fn pty_encoding(&self) -> InputEncoding {
        self.pty_encoding
    }

    /// How the first prompt of a spawned (or resumed with `--prompt`) launch travels:
    /// `Positional` (argv) or the PTY encoding after readiness.
    #[must_use]
    pub fn first_prompt(&self) -> InputEncoding {
        self.first_prompt
    }

    /// Wait between the text part and the submit part of a send unit.
    #[must_use]
    pub fn submit_delay(&self) -> Duration {
        self.submit_delay
    }
}

/// Every row, in the order `(Claude, Macos)`, `(Claude, Windows)`, `(Codex, Macos)`,
/// `(Codex, Windows)`. [`capabilities`] returns the same rows.
pub const CAPABILITIES: [Capabilities; 4] = [
    Capabilities::new(
        Agent::Claude,
        HostOs::Macos,
        InputEncoding::BracketedPaste,
        InputEncoding::BracketedPaste,
        SUBMIT_DELAY,
    ),
    Capabilities::new(
        Agent::Claude,
        HostOs::Windows,
        InputEncoding::BracketedPaste,
        InputEncoding::BracketedPaste,
        SUBMIT_DELAY,
    ),
    Capabilities::new(
        Agent::Codex,
        HostOs::Macos,
        InputEncoding::BracketedPaste,
        InputEncoding::Positional,
        SUBMIT_DELAY,
    ),
    Capabilities::new(
        Agent::Codex,
        HostOs::Windows,
        InputEncoding::WinKeys,
        InputEncoding::Positional,
        SUBMIT_DELAY,
    ),
];

/// The row of `(agent, os)`. Exhaustive over both enums: a new variant fails to compile until
/// it has a row.
#[must_use]
pub fn capabilities(agent: Agent, os: HostOs) -> &'static Capabilities {
    match (agent, os) {
        (Agent::Claude, HostOs::Macos) => &CAPABILITIES[0],
        (Agent::Claude, HostOs::Windows) => &CAPABILITIES[1],
        (Agent::Codex, HostOs::Macos) => &CAPABILITIES[2],
        (Agent::Codex, HostOs::Windows) => &CAPABILITIES[3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: [Agent; 2] = [Agent::Claude, Agent::Codex];
    const OSES: [HostOs; 2] = [HostOs::Macos, HostOs::Windows];

    /// Every row on every host, so a `cfg` choice fails on the other CI runner.
    #[test]
    fn table_rows() {
        use InputEncoding::{BracketedPaste, Positional, WinKeys};
        let want = [
            (Agent::Claude, HostOs::Macos, BracketedPaste, BracketedPaste),
            (
                Agent::Claude,
                HostOs::Windows,
                BracketedPaste,
                BracketedPaste,
            ),
            (Agent::Codex, HostOs::Macos, BracketedPaste, Positional),
            (Agent::Codex, HostOs::Windows, WinKeys, Positional),
        ];
        for (agent, os, pty, first) in want {
            let r = capabilities(agent, os);
            assert_eq!(
                (
                    r.agent(),
                    r.os(),
                    r.pty_encoding(),
                    r.first_prompt(),
                    r.submit_delay()
                ),
                (agent, os, pty, first, Duration::from_millis(300)),
                "{agent:?} {os:?}"
            );
        }
    }

    #[test]
    fn table_and_match_agree() {
        let mut from_match = Vec::new();
        for agent in AGENTS {
            for os in OSES {
                let r = capabilities(agent, os);
                assert_eq!((r.agent(), r.os()), (agent, os));
                from_match.push(*r);
            }
        }
        assert_eq!(from_match, CAPABILITIES);
    }

    #[test]
    fn host_os_from_str() {
        assert_eq!(HostOs::from_os_str("macos"), Some(HostOs::Macos));
        assert_eq!(HostOs::from_os_str("windows"), Some(HostOs::Windows));
        assert_eq!(HostOs::from_os_str("linux"), None);
        assert_eq!(HostOs::from_os_str(""), None);
        assert_eq!(HostOs::from_os_str("Windows"), None);
        assert_eq!(serde_json::to_string(&HostOs::Macos).unwrap(), r#""macos""#);
        assert_eq!(
            serde_json::from_str::<HostOs>(r#""windows""#).unwrap(),
            HostOs::Windows
        );
    }

    #[test]
    fn row_invariants() {
        for r in &CAPABILITIES {
            assert_ne!(r.pty_encoding(), InputEncoding::Positional, "{r:?}");
            assert!(
                r.first_prompt() == InputEncoding::Positional
                    || r.first_prompt() == r.pty_encoding(),
                "{r:?}"
            );
        }
    }
}
