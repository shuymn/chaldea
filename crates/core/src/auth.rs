//! Authorization policy (`docs/mvp-design.md` §2.5, `docs/design/http-surface.md` §2.5):
//! token kinds, actions, the action table, the command list, and the object-scope functions.
//!
//! Pure functions over ids the caller passes in; the daemon reads the thread tree and the
//! stored objects in the transaction that uses them and asks these functions.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{ThreadId, WorkspaceId};
use crate::payload::SurfaceScope;

/// The kind of a token (mvp §2.5 "Token kinds"); the store keeps it in `tokens.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Owner,
    Agent,
    Rescue,
    Hook,
    Extension,
}

impl TokenKind {
    pub const ALL: [TokenKind; 5] = [
        TokenKind::Owner,
        TokenKind::Agent,
        TokenKind::Rescue,
        TokenKind::Hook,
        TokenKind::Extension,
    ];

    /// The public name, as stored and as serialized.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TokenKind::Owner => "owner",
            TokenKind::Agent => "agent",
            TokenKind::Rescue => "rescue",
            TokenKind::Hook => "hook",
            TokenKind::Extension => "extension",
        }
    }
}

/// What a request does, for the action table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    ReadStream,
    ReadBlob,
    ReadThreadList,
    ReadWorktrees,
    ReadAdapters,
    ReadDoctor,
    Spawn,
    Send,
    Stop,
    Resume,
    Close,
    TaskSet,
    InputCancel,
    Key,
    Attach,
    Ready,
    InputResolve,
    MailboxSend,
    WorktreeRemove,
    PostEvents,
    SurfaceAction,
    DoctorRun,
}

impl Action {
    pub const ALL: [Action; 22] = [
        Action::ReadStream,
        Action::ReadBlob,
        Action::ReadThreadList,
        Action::ReadWorktrees,
        Action::ReadAdapters,
        Action::ReadDoctor,
        Action::Spawn,
        Action::Send,
        Action::Stop,
        Action::Resume,
        Action::Close,
        Action::TaskSet,
        Action::InputCancel,
        Action::Key,
        Action::Attach,
        Action::Ready,
        Action::InputResolve,
        Action::MailboxSend,
        Action::WorktreeRemove,
        Action::PostEvents,
        Action::SurfaceAction,
        Action::DoctorRun,
    ];
}

/// The objects a granted action may touch. The handler evaluates it against the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grant {
    /// Any object.
    All,
    /// The token's thread and its descendants.
    SelfPlus,
    /// Spawn: the new thread's parent is the token's thread; never `--role rescue`.
    ChildrenOfSelf,
    /// Task set: the token's thread and its direct children.
    SelfAndDirectChildren,
    /// Input cancel: inputs whose `created_by` is the token's thread.
    CreatedBySelf,
    /// Mailbox send: the recipient is the token thread's parent or one of its children.
    ParentAndChildren,
    /// Worktree remove: the worktree's current owner is the token's thread, no flags.
    OwnWorktree,
    /// `POST /events`: the token's configured scope (PR 20).
    ExtensionScope,
    /// `surface_action`: the surface's scope contains the target (PR 20).
    SurfaceScopeContains,
}

/// The action table, mvp §2.5 "Actions" cell by cell; `None` refuses.
#[must_use]
pub fn grant(kind: TokenKind, action: Action) -> Option<Grant> {
    use Action as A;
    match kind {
        TokenKind::Owner => Some(match action {
            A::SurfaceAction => Grant::SurfaceScopeContains,
            _ => Grant::All,
        }),
        TokenKind::Agent => match action {
            A::ReadStream
            | A::ReadBlob
            | A::ReadThreadList
            | A::ReadWorktrees
            | A::Send
            | A::Stop
            | A::Resume
            | A::Close => Some(Grant::SelfPlus),
            // A static catalog with no host data (mvp §2.5 routes).
            A::ReadAdapters => Some(Grant::All),
            A::Spawn => Some(Grant::ChildrenOfSelf),
            A::TaskSet => Some(Grant::SelfAndDirectChildren),
            A::InputCancel => Some(Grant::CreatedBySelf),
            A::MailboxSend => Some(Grant::ParentAndChildren),
            A::WorktreeRemove => Some(Grant::OwnWorktree),
            A::ReadDoctor
            | A::Key
            | A::Attach
            | A::Ready
            | A::InputResolve
            | A::PostEvents
            | A::SurfaceAction
            | A::DoctorRun => None,
        },
        TokenKind::Rescue => match action {
            A::ReadStream
            | A::ReadBlob
            | A::ReadThreadList
            | A::ReadWorktrees
            | A::ReadAdapters
            | A::ReadDoctor
            | A::PostEvents => Some(Grant::All),
            _ => None,
        },
        TokenKind::Extension => match action {
            A::PostEvents => Some(Grant::ExtensionScope),
            _ => None,
        },
        // The hook token's only route is `/hook`, which checks no action.
        TokenKind::Hook => None,
    }
}

/// One command of `POST /api/commands/{name}` with its fixed action and the PR that
/// implements it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// The wire name (snake_case).
    pub name: &'static str,
    pub action: Action,
    pub owner_pr: u8,
}

/// The commands of mvp §2.4, each with its action from the start, so the guard decides
/// kind and action for every name before any handler exists.
pub static MVP_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "spawn",
        action: Action::Spawn,
        owner_pr: 10,
    },
    CommandSpec {
        name: "resume",
        action: Action::Resume,
        owner_pr: 10,
    },
    CommandSpec {
        name: "send",
        action: Action::Send,
        owner_pr: 10,
    },
    CommandSpec {
        name: "ready",
        action: Action::Ready,
        owner_pr: 10,
    },
    CommandSpec {
        name: "key",
        action: Action::Key,
        owner_pr: 10,
    },
    CommandSpec {
        name: "stop",
        action: Action::Stop,
        owner_pr: 10,
    },
    CommandSpec {
        name: "close",
        action: Action::Close,
        owner_pr: 10,
    },
    CommandSpec {
        name: "input_resolve",
        action: Action::InputResolve,
        owner_pr: 10,
    },
    CommandSpec {
        name: "input_cancel",
        action: Action::InputCancel,
        owner_pr: 10,
    },
    CommandSpec {
        name: "task_set",
        action: Action::TaskSet,
        owner_pr: 16,
    },
    CommandSpec {
        name: "mailbox_send",
        action: Action::MailboxSend,
        owner_pr: 16,
    },
    CommandSpec {
        name: "worktree_remove",
        action: Action::WorktreeRemove,
        owner_pr: 16,
    },
    CommandSpec {
        name: "doctor_run",
        action: Action::DoctorRun,
        owner_pr: 21,
    },
    CommandSpec {
        name: "surface_action",
        action: Action::SurfaceAction,
        owner_pr: 20,
    },
];

/// The command named `name`, if it is one of [`MVP_COMMANDS`].
#[must_use]
pub fn command(name: &str) -> Option<&'static CommandSpec> {
    MVP_COMMANDS.iter().find(|c| c.name == name)
}

/// The thread tree: each thread's parent, as `proj_threads` holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadGraph {
    parent: BTreeMap<ThreadId, Option<ThreadId>>,
    children: BTreeMap<ThreadId, BTreeSet<ThreadId>>,
}

impl ThreadGraph {
    /// Builds the graph from `(thread, parent)` pairs.
    pub fn new(threads: impl IntoIterator<Item = (ThreadId, Option<ThreadId>)>) -> Self {
        let mut g = ThreadGraph::default();
        for (t, p) in threads {
            g.parent.insert(t, p);
            if let Some(p) = p {
                g.children.entry(p).or_default().insert(t);
            }
        }
        g
    }

    #[must_use]
    pub fn contains(&self, t: ThreadId) -> bool {
        self.parent.contains_key(&t)
    }

    #[must_use]
    pub fn parent(&self, t: ThreadId) -> Option<ThreadId> {
        self.parent.get(&t).copied().flatten()
    }

    fn children(&self, t: ThreadId) -> impl Iterator<Item = ThreadId> + '_ {
        self.children.get(&t).into_iter().flatten().copied()
    }
}

/// The thread itself and its descendants ("self+"); empty for a thread not in the graph.
#[must_use]
pub fn self_plus(g: &ThreadGraph, me: ThreadId) -> BTreeSet<ThreadId> {
    let mut out = BTreeSet::new();
    if !g.contains(me) {
        return out;
    }
    let mut todo = vec![me];
    while let Some(t) = todo.pop() {
        // `insert` refusing a repeat also ends a (malformed) cycle.
        if out.insert(t) {
            todo.extend(g.children(t));
        }
    }
    out
}

/// Whether a token whose thread is `me` may act on `target` under `grant`. `target` is the
/// thread the grant is evaluated against: the thread acted on, the requested parent for
/// spawn, the recipient for mailbox send, and the input's creator for input cancel. Grants
/// that are not about threads (`OwnWorktree`, `ExtensionScope`, `SurfaceScopeContains`)
/// return `false` here; see [`may_remove_worktree`] and [`surface_in_scope`].
#[must_use]
pub fn may_act_on_thread(
    g: &ThreadGraph,
    me: Option<ThreadId>,
    grant: Grant,
    target: ThreadId,
) -> bool {
    if grant == Grant::All {
        return true;
    }
    let Some(me) = me else {
        return false;
    };
    if !g.contains(me) || !g.contains(target) {
        return false;
    }
    match grant {
        Grant::All => true,
        Grant::SelfPlus => self_plus(g, me).contains(&target),
        Grant::ChildrenOfSelf | Grant::CreatedBySelf => target == me,
        Grant::SelfAndDirectChildren => target == me || g.parent(target) == Some(me),
        Grant::ParentAndChildren => g.parent(me) == Some(target) || g.parent(target) == Some(me),
        Grant::OwnWorktree | Grant::ExtensionScope | Grant::SurfaceScopeContains => false,
    }
}

/// Whether a token whose thread is `me` may remove a worktree whose current owner is
/// `owner_thread_id` (`None`: the human owner) under `grant`.
#[must_use]
pub fn may_remove_worktree(
    me: Option<ThreadId>,
    grant: Grant,
    owner_thread_id: Option<ThreadId>,
) -> bool {
    match grant {
        Grant::All => true,
        Grant::OwnWorktree => me.is_some() && owner_thread_id == me,
        _ => false,
    }
}

/// Whether a stored surface with scope `surface` is in the caller's scope under `grant`:
/// `All` contains every scope; `ExtensionScope` contains what the token's configured scope
/// (`configured`, PR 20; `None` until then) contains: `Host` every scope, `Workspace` that
/// workspace and its threads, `Thread` that thread. `workspace_of` gives a thread's
/// workspace.
pub fn surface_in_scope(
    grant: Grant,
    configured: Option<&SurfaceScope>,
    surface: &SurfaceScope,
    workspace_of: impl Fn(ThreadId) -> Option<WorkspaceId>,
) -> bool {
    match grant {
        Grant::All => true,
        Grant::ExtensionScope => {
            configured.is_some_and(|c| scope_contains(c, surface, workspace_of))
        }
        _ => false,
    }
}

fn scope_contains(
    outer: &SurfaceScope,
    inner: &SurfaceScope,
    workspace_of: impl Fn(ThreadId) -> Option<WorkspaceId>,
) -> bool {
    match (outer, inner) {
        (SurfaceScope::Host, _) => true,
        (
            SurfaceScope::Workspace { workspace_id: w },
            SurfaceScope::Workspace { workspace_id: v },
        ) => w == v,
        (SurfaceScope::Workspace { workspace_id: w }, SurfaceScope::Thread { thread_id }) => {
            workspace_of(*thread_id).as_ref() == Some(w)
        }
        (SurfaceScope::Thread { thread_id: a }, SurfaceScope::Thread { thread_id: b }) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    fn t(n: u128) -> ThreadId {
        ThreadId::from_u128(n)
    }

    /// mvp §2.5 "Actions", transcribed again: (action, owner, agent, rescue, extension, hook).
    #[test]
    fn grant_table_matches_mvp() {
        use Action as A;
        use Grant as G;
        let all = Some(G::All);
        let sp = Some(G::SelfPlus);
        let table: &[(Action, [Option<Grant>; 5])] = &[
            (A::ReadStream, [all, sp, all, None, None]),
            (A::ReadBlob, [all, sp, all, None, None]),
            (A::ReadThreadList, [all, sp, all, None, None]),
            (A::ReadWorktrees, [all, sp, all, None, None]),
            (A::ReadAdapters, [all, all, all, None, None]),
            (A::ReadDoctor, [all, None, all, None, None]),
            (A::Spawn, [all, Some(G::ChildrenOfSelf), None, None, None]),
            (A::Send, [all, sp, None, None, None]),
            (A::Stop, [all, sp, None, None, None]),
            (A::Resume, [all, sp, None, None, None]),
            (A::Close, [all, sp, None, None, None]),
            (
                A::TaskSet,
                [all, Some(G::SelfAndDirectChildren), None, None, None],
            ),
            (
                A::InputCancel,
                [all, Some(G::CreatedBySelf), None, None, None],
            ),
            (A::Key, [all, None, None, None, None]),
            (A::Attach, [all, None, None, None, None]),
            (A::Ready, [all, None, None, None, None]),
            (A::InputResolve, [all, None, None, None, None]),
            (
                A::MailboxSend,
                [all, Some(G::ParentAndChildren), None, None, None],
            ),
            (
                A::WorktreeRemove,
                [all, Some(G::OwnWorktree), None, None, None],
            ),
            (
                A::PostEvents,
                [all, None, all, Some(G::ExtensionScope), None],
            ),
            (
                A::SurfaceAction,
                [Some(G::SurfaceScopeContains), None, None, None, None],
            ),
            (A::DoctorRun, [all, None, None, None, None]),
        ];
        let kinds = [
            TokenKind::Owner,
            TokenKind::Agent,
            TokenKind::Rescue,
            TokenKind::Extension,
            TokenKind::Hook,
        ];
        assert_eq!(table.len(), Action::ALL.len());
        let listed: BTreeSet<Action> = table.iter().map(|r| r.0).collect();
        assert_eq!(listed, Action::ALL.into_iter().collect());
        for (action, cells) in table {
            for (kind, want) in kinds.iter().zip(cells) {
                assert_eq!(grant(*kind, *action), *want, "{kind:?} × {action:?}");
            }
        }
    }

    #[test]
    fn mvp_commands_match() {
        use Action as A;
        let want: &[(&str, Action, u8)] = &[
            ("spawn", A::Spawn, 10),
            ("resume", A::Resume, 10),
            ("send", A::Send, 10),
            ("ready", A::Ready, 10),
            ("key", A::Key, 10),
            ("stop", A::Stop, 10),
            ("close", A::Close, 10),
            ("input_resolve", A::InputResolve, 10),
            ("input_cancel", A::InputCancel, 10),
            ("task_set", A::TaskSet, 16),
            ("mailbox_send", A::MailboxSend, 16),
            ("worktree_remove", A::WorktreeRemove, 16),
            ("doctor_run", A::DoctorRun, 21),
            ("surface_action", A::SurfaceAction, 20),
        ];
        let got: Vec<(&str, Action, u8)> = MVP_COMMANDS
            .iter()
            .map(|c| (c.name, c.action, c.owner_pr))
            .collect();
        assert_eq!(got, want);
        assert!(command("open").is_none());
        assert_eq!(command("key").map(|c| c.action), Some(A::Key));
    }

    #[test]
    fn token_kind_names() {
        for k in TokenKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
    }

    /// A random forest: thread `i` has a parent among `0..i` or none.
    fn forest() -> impl Strategy<Value = Vec<Option<usize>>> {
        prop::collection::vec(any::<prop::sample::Index>(), 1..24).prop_flat_map(|idx| {
            let n = idx.len();
            prop::collection::vec(any::<bool>(), n).prop_map(move |roots| {
                (0..n)
                    .map(|i| (i > 0 && !roots[i]).then(|| idx[i].index(i)))
                    .collect()
            })
        })
    }

    fn graph(parents: &[Option<usize>]) -> ThreadGraph {
        ThreadGraph::new(
            parents
                .iter()
                .enumerate()
                .map(|(i, p)| (t(i as u128), p.map(|p| t(p as u128)))),
        )
    }

    /// Ancestors by walking parent links, independently of `self_plus`.
    fn is_ancestor_or_self(parents: &[Option<usize>], a: usize, mut d: usize) -> bool {
        loop {
            if d == a {
                return true;
            }
            match parents[d] {
                Some(p) => d = p,
                None => return false,
            }
        }
    }

    proptest! {
        #[test]
        fn self_plus_is_the_descendant_closure(parents in forest(), pick in any::<prop::sample::Index>()) {
            let g = graph(&parents);
            let me = pick.index(parents.len());
            let want: BTreeSet<ThreadId> = (0..parents.len())
                .filter(|&d| is_ancestor_or_self(&parents, me, d))
                .map(|d| t(d as u128))
                .collect();
            prop_assert_eq!(self_plus(&g, t(me as u128)), want);
        }

        #[test]
        fn scope_grants_on_random_trees(
            parents in forest(),
            a in any::<prop::sample::Index>(),
            b in any::<prop::sample::Index>(),
        ) {
            let g = graph(&parents);
            let n = parents.len();
            let (me, target) = (a.index(n), b.index(n));
            let (tm, tt) = (t(me as u128), t(target as u128));
            let cases = [
                (Grant::All, true),
                (Grant::SelfPlus, is_ancestor_or_self(&parents, me, target)),
                (Grant::ChildrenOfSelf, me == target),
                (Grant::CreatedBySelf, me == target),
                (Grant::SelfAndDirectChildren, me == target || parents[target] == Some(me)),
                (Grant::ParentAndChildren, parents[me] == Some(target) || parents[target] == Some(me)),
                (Grant::OwnWorktree, false),
                (Grant::ExtensionScope, false),
                (Grant::SurfaceScopeContains, false),
            ];
            for (grant, want) in cases {
                prop_assert_eq!(may_act_on_thread(&g, Some(tm), grant, tt), want, "{:?}", grant);
            }
            // Without a thread only `All` passes.
            prop_assert!(may_act_on_thread(&g, None, Grant::All, tt));
            prop_assert!(!may_act_on_thread(&g, None, Grant::SelfPlus, tt));
            // An unknown target is never in a scoped grant.
            prop_assert!(!may_act_on_thread(&g, Some(tm), Grant::SelfPlus, t(999)));
        }
    }

    #[test]
    fn own_worktree_only() {
        let (a, b) = (t(1), t(2));
        // A-owned: allowed. B-owned (B a child of A): refused. After B's close the owner
        // passes to A: allowed.
        assert!(may_remove_worktree(Some(a), Grant::OwnWorktree, Some(a)));
        assert!(!may_remove_worktree(Some(a), Grant::OwnWorktree, Some(b)));
        assert!(may_remove_worktree(Some(a), Grant::OwnWorktree, Some(a)));
        // Human-owned, or no thread: refused; the owner grant removes anything.
        assert!(!may_remove_worktree(Some(a), Grant::OwnWorktree, None));
        assert!(!may_remove_worktree(None, Grant::OwnWorktree, None));
        assert!(may_remove_worktree(None, Grant::All, Some(b)));
        assert!(!may_remove_worktree(Some(a), Grant::SelfPlus, Some(a)));
    }

    #[test]
    fn surface_scopes() {
        let w = WorkspaceId::from("w1");
        let other = WorkspaceId::from("w2");
        let ws = |t: ThreadId| (t == ThreadId::from_u128(1)).then(|| WorkspaceId::from("w1"));
        let host = SurfaceScope::Host;
        let wsp = SurfaceScope::Workspace {
            workspace_id: w.clone(),
        };
        let th1 = SurfaceScope::Thread { thread_id: t(1) };
        let th2 = SurfaceScope::Thread { thread_id: t(2) };
        for s in [&host, &wsp, &th1] {
            assert!(surface_in_scope(Grant::All, None, s, ws));
            assert!(!surface_in_scope(Grant::ExtensionScope, None, s, ws));
            assert!(surface_in_scope(Grant::ExtensionScope, Some(&host), s, ws));
            assert!(!surface_in_scope(Grant::SelfPlus, Some(&host), s, ws));
        }
        assert!(surface_in_scope(
            Grant::ExtensionScope,
            Some(&wsp),
            &th1,
            ws
        ));
        assert!(!surface_in_scope(
            Grant::ExtensionScope,
            Some(&wsp),
            &th2,
            ws
        ));
        assert!(!surface_in_scope(
            Grant::ExtensionScope,
            Some(&wsp),
            &host,
            ws
        ));
        let other_ws = SurfaceScope::Workspace {
            workspace_id: other,
        };
        assert!(!surface_in_scope(
            Grant::ExtensionScope,
            Some(&wsp),
            &other_ws,
            ws
        ));
        assert!(surface_in_scope(
            Grant::ExtensionScope,
            Some(&th1),
            &th1,
            ws
        ));
        assert!(!surface_in_scope(
            Grant::ExtensionScope,
            Some(&th1),
            &th2,
            ws
        ));
    }
}
