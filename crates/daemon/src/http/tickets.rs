//! WebSocket tickets in memory (design §2.4 "Tickets", "Redemption"): keyed by hash, peeked
//! at the credential step, and removed only after every fallible check, under one mutex.
//! A restart drops them; losing one only makes its holder mint another.
//!
//! PR 7b adds the minting route and its caps; PR 7a has the store the guard and the upgrade
//! admission use.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{ThreadId, TokenId};

#[cfg(test)]
use super::auth::{Secret, TICKET_PREFIX};

/// What a ticket may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketPurpose {
    /// `GET /api/ws`.
    Events,
    /// `GET /api/attach/{thread}` for that thread.
    Attach(ThreadId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TicketEntry {
    pub(crate) token_id: TokenId,
    pub(crate) token_sha256: [u8; 32],
    pub(crate) kind: TokenKind,
    pub(crate) thread: Option<ThreadId>,
    pub(crate) purpose: TicketPurpose,
    pub(crate) expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct TicketStore {
    map: Mutex<HashMap<[u8; 32], TicketEntry>>,
}

impl TicketStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], TicketEntry>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores a new ticket for `entry` and returns its secret (the minting route arrives
    /// with PR 7b; until then only tests mint).
    #[cfg(test)]
    pub(crate) fn mint(&self, entry: TicketEntry) -> Secret {
        let secret = Secret::generate(TICKET_PREFIX);
        self.lock().insert(secret.sha256(), entry);
        secret
    }

    /// The entry if present and `now < expires_at`; nothing is removed.
    pub(crate) fn peek(&self, sha256: &[u8; 32], now: Instant) -> Option<TicketEntry> {
        self.lock()
            .get(sha256)
            .filter(|e| now < e.expires_at)
            .cloned()
    }

    /// Removes the entry only if it is still present and `now < expires_at`, with `now`
    /// sampled by the caller at the removal; the caller whose removal returns it has it.
    pub(crate) fn remove_if_valid(&self, sha256: &[u8; 32], now: Instant) -> Option<TicketEntry> {
        let mut map = self.lock();
        if map.get(sha256).is_some_and(|e| now < e.expires_at) {
            map.remove(sha256)
        } else {
            None
        }
    }
}
