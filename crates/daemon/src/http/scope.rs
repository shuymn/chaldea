//! What a token may read (design §2.5 "Readable streams", "Per-resource filtering"), decided
//! inside the read transaction that serves the reply.

use std::collections::BTreeSet;

use chaldea_core::auth::{Action, Grant, ThreadGraph, grant, self_plus};
use chaldea_core::ids::{StreamId, ThreadId};

use super::auth::Authorized;
use crate::store::{ReadTx, StoreError};

/// The readable set of one principal at one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadScope {
    /// `owner` and `rescue`: every stream, thread, and worktree.
    All,
    /// `agent`: the token's thread and its descendants; never the `host` stream.
    Threads(BTreeSet<ThreadId>),
}

impl ReadScope {
    /// The scope of `auth` for `action` (one of the read actions), from the thread tree in
    /// `tx`'s snapshot. A kind without the grant reads nothing.
    pub(crate) fn of(
        tx: &ReadTx<'_>,
        auth: &Authorized,
        action: Action,
    ) -> Result<ReadScope, StoreError> {
        match grant(auth.kind, action) {
            Some(Grant::All) => Ok(ReadScope::All),
            Some(Grant::SelfPlus) => {
                let Some(me) = auth.thread else {
                    return Ok(ReadScope::Threads(BTreeSet::new()));
                };
                let g = ThreadGraph::new(
                    tx.threads()?
                        .into_iter()
                        .map(|t| (t.thread_id, t.parent_thread_id)),
                );
                Ok(ReadScope::Threads(self_plus(&g, me)))
            }
            _ => Ok(ReadScope::Threads(BTreeSet::new())),
        }
    }

    pub(crate) fn thread(&self, t: ThreadId) -> bool {
        match self {
            ReadScope::All => true,
            ReadScope::Threads(ts) => ts.contains(&t),
        }
    }

    /// Whether the token may read `s`; existence is checked separately.
    pub(crate) fn stream(&self, s: StreamId) -> bool {
        match (self, s) {
            (ReadScope::All, _) => true,
            (ReadScope::Threads(_), StreamId::Host) => false,
            (ReadScope::Threads(ts), StreamId::Thread(t)) => ts.contains(&t),
        }
    }

    /// Whether `s` exists in `tx`'s snapshot and the token may read it: the one check of
    /// every stream answer (404 otherwise, one body for both).
    pub(crate) fn readable_stream(&self, tx: &ReadTx<'_>, s: StreamId) -> Result<bool, StoreError> {
        Ok(self.stream(s) && tx.stream_exists(s)?)
    }
}
