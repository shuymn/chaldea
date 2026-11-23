//! WebSocket tickets and pair codes in memory (design §2.4 "Tickets", "Redemption", "Pair
//! codes"): keyed by hash, each store under one mutex. Tickets are peeked at the credential
//! step and removed only after every fallible check; pair codes are removed at the exchange.
//! Expired entries are purged on every insert, and the caps are counted and the insert done
//! under the same lock. A restart drops them; losing one only makes its holder ask again.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{ThreadId, TokenId};

use super::MonotonicClock;
use super::auth::{PAIR_CODE_PREFIX, Secret, TICKET_PREFIX};

/// Tickets held at once (design §2.4).
pub const MAX_TICKETS: usize = 1024;
/// Tickets held at once for one token.
pub const MAX_TICKETS_PER_TOKEN: usize = 32;
/// Pair codes held at once.
pub const MAX_PAIR_CODES: usize = 8;

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

/// A cap was reached: 503 `busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Full;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
pub(crate) struct TicketStore {
    map: Mutex<HashMap<[u8; 32], TicketEntry>>,
}

impl TicketStore {
    /// Purges expired entries, then stores a new ticket for `entry` unless a cap is reached
    /// (`MAX_TICKETS` in all, `MAX_TICKETS_PER_TOKEN` for `entry`'s token), all under one
    /// lock; returns the ticket's secret.
    pub(crate) fn mint(&self, entry: TicketEntry, now: Instant) -> Result<Secret, Full> {
        let mut map = lock(&self.map);
        map.retain(|_, e| now < e.expires_at);
        let mine = map
            .values()
            .filter(|e| e.token_id == entry.token_id)
            .count();
        if map.len() >= MAX_TICKETS || mine >= MAX_TICKETS_PER_TOKEN {
            return Err(Full);
        }
        let secret = Secret::generate(TICKET_PREFIX);
        map.insert(secret.sha256(), entry);
        Ok(secret)
    }

    /// The entry if present and `now < expires_at`; nothing is removed.
    pub(crate) fn peek(&self, sha256: &[u8; 32], now: Instant) -> Option<TicketEntry> {
        lock(&self.map)
            .get(sha256)
            .filter(|e| now < e.expires_at)
            .cloned()
    }

    /// Removes the entry only if it is still present and `now < expires_at`, with `now`
    /// sampled by the caller at the removal; the caller whose removal returns it has it.
    pub(crate) fn remove_if_valid(&self, sha256: &[u8; 32], now: Instant) -> Option<TicketEntry> {
        let mut map = lock(&self.map);
        if map.get(sha256).is_some_and(|e| now < e.expires_at) {
            map.remove(sha256)
        } else {
            None
        }
    }

    /// Tickets held, expired ones included until the next insert purges them.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.map).len()
    }
}

/// A pair code: the hash of the token that minted it, and its expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PairCodeEntry {
    pub(crate) issuer_sha256: [u8; 32],
    pub(crate) expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct PairCodes {
    map: Mutex<HashMap<[u8; 32], PairCodeEntry>>,
    /// Run once by the next `take` before it takes the lock (design §7 fault point).
    #[cfg(test)]
    pub(crate) before_take_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PairCodes {
    /// Purges expired codes, then stores a new one unless `MAX_PAIR_CODES` are held, under
    /// one lock; returns the code.
    pub(crate) fn mint(&self, entry: PairCodeEntry, now: Instant) -> Result<Secret, Full> {
        let mut map = lock(&self.map);
        map.retain(|_, e| now < e.expires_at);
        if map.len() >= MAX_PAIR_CODES {
            return Err(Full);
        }
        let secret = Secret::generate(PAIR_CODE_PREFIX);
        map.insert(secret.sha256(), entry);
        Ok(secret)
    }

    /// Removes the code only if it is present and `now < expires_at`, with `now` read from
    /// `clock` under the lock, so expiry is decided at the removal: the code is spent from
    /// here on, whatever the exchange answers.
    pub(crate) fn take(
        &self,
        sha256: &[u8; 32],
        clock: &dyn MonotonicClock,
    ) -> Option<PairCodeEntry> {
        #[cfg(test)]
        if let Some(f) = lock(&self.before_take_lock).take() {
            f();
        }
        let mut map = lock(&self.map);
        let now = clock.now();
        if map.get(sha256).is_some_and(|e| now < e.expires_at) {
            map.remove(sha256)
        } else {
            None
        }
    }
}
