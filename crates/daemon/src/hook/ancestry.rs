//! Codex binding eligibility (`docs/design/hook-ingest.md` §2.5 "Eligibility"): pure,
//! decided in the job against the launch row.
//!
//! The chain is the emitter's ancestry walked at intake, a heuristic over current parents
//! and pids (§2.5 "What the walk is"). It decides one hook's eligibility and nothing else.

use chaldea_pty_runtime::process::{Ancestry, AncestryEnd};
use serde::{Deserialize, Serialize};

/// One kept link of the walk, as stored in a buffered delivery's meta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub pid: u32,
    pub start_time: String,
    pub name: String,
}

/// The walk of one request: its kept links (`links[0]` is the emitter pid's process) and
/// how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chain {
    pub links: Vec<Link>,
    /// `root`, `limit`, `gone:<pid>`, `unreadable:<pid>`, `parent_newer:<pid>`, or
    /// `no_pid` (no usable `X-Chaldea-Emitter-Pid`); recorded only.
    pub end: String,
}

impl Chain {
    /// The chain of a request without a usable emitter pid: empty, never eligible.
    #[must_use]
    pub fn no_pid() -> Chain {
        Chain {
            links: Vec::new(),
            end: "no_pid".into(),
        }
    }

    /// The kept links of `a` and its end.
    #[must_use]
    pub fn from_ancestry(a: Ancestry) -> Chain {
        let end = match a.end {
            AncestryEnd::Root => "root".to_owned(),
            AncestryEnd::Limit => "limit".to_owned(),
            AncestryEnd::Gone(p) => format!("gone:{p}"),
            AncestryEnd::Unreadable(p) => format!("unreadable:{p}"),
            AncestryEnd::ParentNewer(p) => format!("parent_newer:{p}"),
        };
        Chain {
            links: a
                .links
                .into_iter()
                .map(|e| Link {
                    pid: e.identity.pid,
                    start_time: e.identity.start_time,
                    name: e.name,
                })
                .collect(),
            end,
        }
    }
}

/// Whether `links` is eligible for the launch whose recorded identity is `(l_pid,
/// l_start)`: `links[0]` runs the emitter binary; some link `k ≥ 1` is the launch; and the
/// nearest link `j ≥ 1` whose name is a CLI name (`agent_name` or `links[k].name`) is `k`.
/// A walk that ended before reaching the launch has no such `k` and fails closed; an empty
/// chain is never eligible.
#[must_use]
pub fn eligible(
    links: &[Link],
    l_pid: u32,
    l_start: &str,
    agent_name: &str,
    emitter_name: &str,
) -> bool {
    let Some(first) = links.first() else {
        return false;
    };
    if first.name != emitter_name {
        return false;
    }
    let Some(k) = links
        .iter()
        .skip(1)
        .position(|x| x.pid == l_pid && x.start_time == l_start)
        .map(|i| i + 1)
    else {
        return false;
    };
    let launched = links[k].name.as_str();
    let nearest = links
        .iter()
        .skip(1)
        .position(|x| x.name == agent_name || x.name == launched)
        .map(|i| i + 1);
    nearest == Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(pid: u32, start: &str, name: &str) -> Link {
        Link {
            pid,
            start_time: start.into(),
            name: name.into(),
        }
    }

    const L: (u32, &str) = (100, "1000");

    fn ok(links: &[Link]) -> bool {
        eligible(links, L.0, L.1, "codex", "chaldea")
    }

    /// The eligibility table (PR9-CL-26, daemon half): ends before `L` for each end kind,
    /// an end above `L`, a nested CLI of the same name, another agent name, an empty
    /// chain, and a first link that does not run the emitter.
    #[test]
    fn eligibility() {
        let emitter = link(300, "3000", "chaldea");
        let shell = link(200, "2000", "sh");
        let l = link(L.0, L.1, "codex");
        let parent = link(50, "500", "zsh");
        // The plain chain, and one whose walk continued above L.
        assert!(ok(&[emitter.clone(), l.clone()]));
        assert!(ok(&[
            emitter.clone(),
            shell.clone(),
            l.clone(),
            parent.clone()
        ]));
        // Each end kind before L leaves L out of the kept links (the walk stopped at the
        // shell): not eligible whatever the end.
        assert!(!ok(&[emitter.clone(), shell.clone()]));
        assert!(!ok(std::slice::from_ref(&emitter)));
        // L's identity must match on both fields.
        assert!(!ok(&[emitter.clone(), link(L.0, "999", "codex")]));
        assert!(!ok(&[emitter.clone(), link(101, L.1, "codex")]));
        // A nested CLI of the same name between the emitter and L.
        let nested = link(250, "2500", "codex");
        assert!(!ok(&[emitter.clone(), nested.clone(), l.clone()]));
        // A launched binary of another name: the agent name still counts as a CLI name.
        let fake = link(L.0, L.1, "chaldea-fake-cli");
        assert!(ok(&[emitter.clone(), fake.clone()]));
        assert!(!ok(&[
            emitter.clone(),
            link(250, "2500", "chaldea-fake-cli"),
            fake.clone()
        ]));
        assert!(!ok(&[
            emitter.clone(),
            link(250, "2500", "codex"),
            fake.clone()
        ]));
        // An unrecognised name between them does not count.
        assert!(ok(&[
            emitter.clone(),
            link(250, "2500", "codex-dev"),
            l.clone()
        ]));
        // Another agent's name is not a CLI name of this launch.
        assert!(eligible(
            &[
                emitter.clone(),
                link(250, "2500", "codex"),
                link(L.0, L.1, "claude")
            ],
            L.0,
            L.1,
            "claude",
            "chaldea"
        ));
        // Empty chain; first link not the emitter binary; L as link 0.
        assert!(!ok(&[]));
        assert!(!ok(&[link(300, "3000", "codex"), l.clone()]));
        assert!(!ok(std::slice::from_ref(&l)));
    }
}
