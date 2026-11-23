//! Process lookup and the ancestry walk (`docs/design/hook-ingest.md` §2.5): one process as
//! the OS reports it, and the chain of its current parents.
//!
//! The walk is a heuristic over current parents and pids, read once: a parent link names the
//! current parent (macOS, changed by reparenting and a debugger attach) or the creator's pid
//! without its generation (Windows). It stops at the first parent that is gone, unreadable,
//! or newer than its child, and keeps every link read before that. It is evidence for hook
//! binding only, never for a kill.
//!
//! The decision logic is [`walk`] over a [`Table`], unit-tested with synthetic tables on
//! every OS; the OS reads are in `unix::table` (libproc) and `windows::process` (one Toolhelp
//! snapshot and one handle per link).

use crate::backend::ProcessIdentity;

/// One live process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    /// The backend's identity format: `(pid, start_time)` compares equal with the identity
    /// of a launch's `launch_started`. `group` is display only (the pgid on macOS, empty on
    /// Windows); compare `(pid, start_time)`, not the whole identity.
    pub identity: ProcessIdentity,
    /// The current parent's pid (macOS `pbi_ppid`, Windows `th32ParentProcessID`).
    pub ppid: u32,
    /// The image name, ASCII-lowercased, a trailing `.exe` removed. macOS: `pbi_name` (at
    /// most 31 characters), else `pbi_comm` (at most 15); Windows: the Toolhelp entry's
    /// `szExeFile`. Compare it only with names read by this function on the same host.
    pub name: String,
}

/// The outcome of one lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Live(ProcessEntry),
    /// No such process, or it has exited (a zombie on macOS; a signalled handle on Windows).
    Gone,
    /// The process may exist, but its record cannot be read (refused, or a failed wait).
    Unreadable,
}

/// Why a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestryEnd {
    /// The last link's parent pid is 0 or its own pid.
    Root,
    /// `max` links were read.
    Limit,
    /// This pid (the parent, or the starting pid itself) is gone.
    Gone(u32),
    /// This pid (the parent, or the starting pid itself) cannot be read.
    Unreadable(u32),
    /// This parent pid names a process started after its child: a pid that a later process
    /// took.
    ParentNewer(u32),
}

/// A walk's result: `links[0]` is the starting pid itself (empty when it is not live), each
/// next link the previous one's parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestry {
    pub links: Vec<ProcessEntry>,
    pub end: AncestryEnd,
}

/// The process with this pid.
#[must_use]
pub fn lookup(pid: u32) -> Lookup {
    imp::lookup(pid)
}

/// The chain of `pid` and its parents, at most `max` links. Blocking: one libproc call per
/// link on macOS; on Windows one Toolhelp snapshot and one handle per link, each closed
/// before the next is opened.
#[must_use]
pub fn ancestry(pid: u32, max: usize) -> Ancestry {
    walk(&mut imp::table(), pid, max)
}

/// The lookups a walk reads.
pub(crate) trait Table {
    fn lookup(&mut self, pid: u32) -> Lookup;
}

/// The walk of §2.5: reads `pid`, then follows the last link's `ppid` until the root, `max`
/// links, a parent that is gone or unreadable, or a parent newer than its child.
pub(crate) fn walk(t: &mut dyn Table, pid: u32, max: usize) -> Ancestry {
    let mut links: Vec<ProcessEntry> = Vec::new();
    if max == 0 {
        return Ancestry {
            links,
            end: AncestryEnd::Limit,
        };
    }
    let mut child = match t.lookup(pid) {
        Lookup::Live(e) => e,
        Lookup::Gone => return ended(links, AncestryEnd::Gone(pid)),
        Lookup::Unreadable => return ended(links, AncestryEnd::Unreadable(pid)),
    };
    loop {
        let ppid = child.ppid;
        if ppid == 0 || ppid == child.identity.pid {
            links.push(child);
            return ended(links, AncestryEnd::Root);
        }
        if links.len() + 1 >= max {
            links.push(child);
            return ended(links, AncestryEnd::Limit);
        }
        let end = match t.lookup(ppid) {
            Lookup::Gone => AncestryEnd::Gone(ppid),
            Lookup::Unreadable => AncestryEnd::Unreadable(ppid),
            Lookup::Live(parent) if newer(&parent, &child) => AncestryEnd::ParentNewer(ppid),
            Lookup::Live(parent) => {
                links.push(std::mem::replace(&mut child, parent));
                continue;
            }
        };
        links.push(child);
        return ended(links, end);
    }
}

fn ended(links: Vec<ProcessEntry>, end: AncestryEnd) -> Ancestry {
    Ancestry { links, end }
}

/// Whether `parent` started after `child`. Both stamps are decimal integers of one clock
/// (µs on macOS, 100 ns on Windows); a stamp that does not parse fails closed (newer).
fn newer(parent: &ProcessEntry, child: &ProcessEntry) -> bool {
    match (
        parent.identity.start_time.parse::<u64>(),
        child.identity.start_time.parse::<u64>(),
    ) {
        (Ok(p), Ok(c)) => p > c,
        _ => true,
    }
}

/// An image name as [`ProcessEntry::name`] holds it: ASCII-lowercased, a trailing `.exe`
/// removed.
pub(crate) fn normalize(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => lower,
    }
}

/// A macOS process record (`unix::table::Lookup`) in a form every OS can build, so the
/// mapping (a zombie is gone) is unit-tested everywhere.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "built by the macOS lookup and the unit tests")
)]
pub(crate) enum UnixRecord {
    Live {
        pid: u32,
        ppid: u32,
        pgid: u32,
        /// µs since the Unix epoch.
        start: u64,
        /// The raw image name, before [`normalize`].
        name: String,
    },
    Zombie,
    Gone,
    Unreadable,
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "used by the macOS lookup and the unit tests")
)]
pub(crate) fn from_unix(r: UnixRecord) -> Lookup {
    match r {
        UnixRecord::Live {
            pid,
            ppid,
            pgid,
            start,
            name,
        } => Lookup::Live(ProcessEntry {
            identity: ProcessIdentity {
                pid,
                start_time: start.to_string(),
                group: pgid.to_string(),
            },
            ppid,
            name: normalize(&name),
        }),
        UnixRecord::Zombie | UnixRecord::Gone => Lookup::Gone,
        UnixRecord::Unreadable => Lookup::Unreadable,
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Lookup, Table, UnixRecord, from_unix};
    use crate::unix::table::{self, Lookup as Raw};

    pub(super) struct Libproc;

    impl Table for Libproc {
        fn lookup(&mut self, pid: u32) -> Lookup {
            lookup(pid)
        }
    }

    pub(super) fn table() -> Libproc {
        Libproc
    }

    pub(super) fn lookup(pid: u32) -> Lookup {
        from_unix(match table::lookup(pid) {
            Raw::Live(p) => {
                let len = p.name.iter().position(|b| *b == 0).unwrap_or(p.name.len());
                UnixRecord::Live {
                    pid,
                    ppid: p.ppid,
                    pgid: p.pgid,
                    start: p.start,
                    name: String::from_utf8_lossy(&p.name[..len]).into_owned(),
                }
            }
            Raw::Zombie(_) => UnixRecord::Zombie,
            Raw::Gone => UnixRecord::Gone,
            Raw::Unreadable => UnixRecord::Unreadable,
        })
    }
}

#[cfg(windows)]
mod imp {
    pub(super) use crate::windows::process::{lookup, table};
}

/// No native backend on this OS: every process is unreadable.
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::{Lookup, Table};

    pub(super) struct Unsupported;

    impl Table for Unsupported {
        fn lookup(&mut self, _pid: u32) -> Lookup {
            Lookup::Unreadable
        }
    }

    pub(super) fn table() -> Unsupported {
        Unsupported
    }

    pub(super) fn lookup(_pid: u32) -> Lookup {
        Lookup::Unreadable
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A synthetic process table: macOS-shaped records by pid, and every pid looked up.
    #[derive(Default)]
    struct Synthetic {
        procs: HashMap<u32, (u32, u64, &'static str)>,
        zombies: Vec<u32>,
        unreadable: Vec<u32>,
        read: Vec<u32>,
    }

    impl Synthetic {
        /// `(pid, ppid, start, name)` rows.
        fn of(rows: &[(u32, u32, u64, &'static str)]) -> Synthetic {
            Synthetic {
                procs: rows
                    .iter()
                    .map(|&(pid, ppid, start, name)| (pid, (ppid, start, name)))
                    .collect(),
                ..Synthetic::default()
            }
        }
    }

    impl Table for Synthetic {
        fn lookup(&mut self, pid: u32) -> Lookup {
            self.read.push(pid);
            from_unix(if self.unreadable.contains(&pid) {
                UnixRecord::Unreadable
            } else if self.zombies.contains(&pid) {
                UnixRecord::Zombie
            } else if let Some(&(ppid, start, name)) = self.procs.get(&pid) {
                UnixRecord::Live {
                    pid,
                    ppid,
                    pgid: pid,
                    start,
                    name: name.to_owned(),
                }
            } else {
                UnixRecord::Gone
            })
        }
    }

    fn pids(a: &Ancestry) -> Vec<u32> {
        a.links.iter().map(|l| l.identity.pid).collect()
    }

    /// emitter 40 → cli 30 → launch 20 → daemon 10 → root 1 (ppid 0).
    fn chain() -> Synthetic {
        Synthetic::of(&[
            (40, 30, 400, "Chaldea.EXE"),
            (30, 20, 300, "codex"),
            (20, 10, 200, "codex"),
            (10, 1, 100, "chaldea"),
            (1, 0, 10, "launchd"),
        ])
    }

    #[test]
    fn walk_to_root() {
        let mut t = chain();
        let a = walk(&mut t, 40, 16);
        assert_eq!(a.end, AncestryEnd::Root);
        assert_eq!(pids(&a), [40, 30, 20, 10, 1]);
        assert_eq!(a.links[0].name, "chaldea");
        assert_eq!(a.links[0].identity.start_time, "400");
        assert_eq!(a.links[0].ppid, 30);
        // A link that is its own parent is a root too.
        let mut t = Synthetic::of(&[(5, 5, 1, "x")]);
        let a = walk(&mut t, 5, 16);
        assert_eq!((pids(&a), a.end), (vec![5], AncestryEnd::Root));
        assert_eq!(t.read, [5]);
    }

    #[test]
    fn depth_limit() {
        let mut t = chain();
        let a = walk(&mut t, 40, 3);
        assert_eq!(a.end, AncestryEnd::Limit);
        assert_eq!(pids(&a), [40, 30, 20]);
        // No lookup beyond the limit.
        assert_eq!(t.read, [40, 30, 20]);
        let mut t = chain();
        let a = walk(&mut t, 40, 1);
        assert_eq!((pids(&a), a.end), (vec![40], AncestryEnd::Limit));
        let mut t = chain();
        let a = walk(&mut t, 40, 0);
        assert_eq!((pids(&a), a.end), (vec![], AncestryEnd::Limit));
        assert!(t.read.is_empty());
    }

    #[test]
    fn gone_parent() {
        let mut t = chain();
        t.procs.remove(&20);
        let a = walk(&mut t, 40, 16);
        assert_eq!(a.end, AncestryEnd::Gone(20));
        assert_eq!(pids(&a), [40, 30]);
        // The starting pid itself gone: an empty chain.
        let a = walk(&mut chain(), 99, 16);
        assert_eq!((pids(&a), a.end), (vec![], AncestryEnd::Gone(99)));
    }

    #[test]
    fn zombie_is_gone() {
        let mut t = chain();
        t.zombies.push(20);
        let a = walk(&mut t, 40, 16);
        assert_eq!(a.end, AncestryEnd::Gone(20));
        assert_eq!(pids(&a), [40, 30]);
        let mut t = chain();
        t.zombies.push(40);
        assert_eq!(walk(&mut t, 40, 16).end, AncestryEnd::Gone(40));
    }

    #[test]
    fn unreadable_above_l_keeps_prefix() {
        let mut t = chain();
        t.unreadable.push(10);
        let a = walk(&mut t, 40, 16);
        assert_eq!(a.end, AncestryEnd::Unreadable(10));
        assert_eq!(pids(&a), [40, 30, 20]);
        assert_eq!(a.links[2].identity.start_time, "200");
        let mut t = chain();
        t.unreadable.push(40);
        let a = walk(&mut t, 40, 16);
        assert_eq!((pids(&a), a.end), (vec![], AncestryEnd::Unreadable(40)));
    }

    #[test]
    fn parent_newer_ends_walk() {
        // Pid 20 was taken by a process started after its "child" 30.
        let mut t = chain();
        t.procs.insert(20, (10, 301, "codex"));
        let a = walk(&mut t, 40, 16);
        assert_eq!(a.end, AncestryEnd::ParentNewer(20));
        assert_eq!(pids(&a), [40, 30]);
        // Equal start stamps are not newer.
        let mut t = chain();
        t.procs.insert(20, (10, 300, "codex"));
        assert_eq!(walk(&mut t, 40, 16).end, AncestryEnd::Root);
    }

    #[test]
    fn unparsable_start_fails_closed() {
        let entry = |start: &str| ProcessEntry {
            identity: ProcessIdentity {
                pid: 1,
                start_time: start.to_owned(),
                group: String::new(),
            },
            ppid: 0,
            name: String::new(),
        };
        assert!(newer(&entry(""), &entry("5")));
        assert!(newer(&entry("5"), &entry("x")));
        assert!(!newer(&entry("5"), &entry("5")));
        assert!(!newer(&entry("4"), &entry("5")));
        assert!(newer(&entry("10"), &entry("9")));
    }

    #[test]
    fn names_are_normalized() {
        assert_eq!(normalize("Chaldea.EXE"), "chaldea");
        assert_eq!(normalize("chaldea-fake-cli.exe"), "chaldea-fake-cli");
        assert_eq!(normalize("codex"), "codex");
        assert_eq!(normalize("a.exe.exe"), "a.exe");
        assert_eq!(normalize("Ä.exe"), "Ä");
    }
}
