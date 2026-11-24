//! Upgrade admission (design §2.4 "Redemption" steps 3–6, §2.6 "Socket ownership").
//!
//! Every fallible check runs before the ticket is removed: the handshake, the socket task's
//! registration, and the final admission read. The socket task is registered before the
//! `101`, so every socket is in the registry from before `101` to its exit; axum's own
//! upgrade task only hands the socket over. Until the ticket is removed the socket task
//! waits on its admission, which ends it by refusing; only then does its hand-over bound
//! start, so a spent ticket always has a task waiting for its socket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, Request, Response, header};
use chaldea_core::auth::TokenKind;
use chaldea_core::ids::{ThreadId, TokenId};
use tokio::sync::{oneshot, watch};

use super::Shared;
use super::auth::Principal;
use super::ctx::verify_until;
use super::error::ApiError;
use super::registry::TaskKind;
use super::routes::UpgradeHandler;
use super::tickets::TicketPurpose;

/// The daemon's shutdown signal as a registered task sees it.
#[derive(Clone)]
pub struct ShutdownSignal(watch::Receiver<bool>);

impl ShutdownSignal {
    pub(crate) fn new(rx: watch::Receiver<bool>) -> Self {
        ShutdownSignal(rx)
    }

    /// Resolves once shutdown has begun (at once if it already has).
    pub async fn wait(&mut self) {
        let _ = self.0.wait_for(|s| *s).await;
    }

    #[must_use]
    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }
}

/// Who an admitted upgrade belongs to: the ticket's token (id and hash, no plaintext).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeGrant {
    pub token_id: TokenId,
    pub token_sha256: [u8; 32],
    pub kind: TokenKind,
    pub thread: Option<ThreadId>,
    pub purpose: TicketPurpose,
}

/// What an upgrade handler runs with, inside its registered socket task.
pub struct UpgradedSocket {
    pub socket: WebSocket,
    pub grant: UpgradeGrant,
    pub shutdown: ShutdownSignal,
    pub(crate) shared: Arc<Shared>,
}

#[derive(Default)]
struct SlotCounts {
    total: usize,
    per_token: HashMap<TokenId, usize>,
}

/// The socket caps (design §2.4 step 3, §5): counted and taken under one lock; a slot is
/// held by its socket task and released when the task ends or the upgrade is refused.
#[derive(Default, Clone)]
pub(crate) struct SocketSlots(Arc<Mutex<SlotCounts>>);

/// One taken socket slot; dropping it frees the slot.
pub(crate) struct SocketSlot {
    slots: SocketSlots,
    token: TokenId,
}

impl SocketSlots {
    /// A slot for `token`, unless `max` sockets in all or `per_token` for it are held.
    pub(crate) fn take(&self, token: TokenId, max: usize, per_token: usize) -> Option<SocketSlot> {
        let mut c = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mine = c.per_token.get(&token).copied().unwrap_or(0);
        if c.total >= max || mine >= per_token {
            return None;
        }
        c.total += 1;
        c.per_token.insert(token, mine + 1);
        Some(SocketSlot {
            slots: self.clone(),
            token,
        })
    }

    /// Slots held.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).total
    }
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut c = self.slots.0.lock().unwrap_or_else(PoisonError::into_inner);
        c.total -= 1;
        if let Some(n) = c.per_token.get_mut(&self.token) {
            *n -= 1;
            if *n == 0 {
                c.per_token.remove(&self.token);
            }
        }
    }
}

/// Whether any `Connection` token is `close`, in any value, UTF-8 or not: an upgrade asking
/// to close cannot keep the connection for the socket.
fn asks_close(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|v| v.as_bytes().split(|b| *b == b','))
        .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"close"))
}

/// Steps 3–6 of the redemption; every refusal keeps the ticket and releases the task.
pub(crate) async fn admit(
    shared: Arc<Shared>,
    principal: Principal,
    ticket_sha: [u8; 32],
    purpose: TicketPurpose,
    deadline: Instant,
    req: Request<Body>,
    handler: Arc<dyn UpgradeHandler>,
) -> Result<Response<Body>, ApiError> {
    let (mut parts, _body) = req.into_parts();
    // 3. The handshake (axum's checks), and no `close` token.
    let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return Err(ApiError::bad_request());
    };
    if asks_close(&parts.headers) {
        return Err(ApiError::bad_request());
    }
    let wcfg = &shared.cfg.ws;
    let ws = ws
        .max_message_size(wcfg.max_message_bytes)
        .max_frame_size(wcfg.max_message_bytes)
        .max_write_buffer_size(wcfg.max_write_buffer_bytes);
    // The socket slot, held by the socket task from here to its exit.
    let slot = shared
        .sockets
        .take(
            principal.token_id,
            wcfg.max_sockets,
            wcfg.max_sockets_per_token,
        )
        .ok_or_else(ApiError::busy)?;
    // The socket task, registered before the ticket is spent. It waits on `admitted` while
    // admission runs: a refusal below drops the sender and ends the task at once. The
    // hand-over bound starts only once the ticket is removed.
    let (admitted_tx, admitted) = oneshot::channel::<()>();
    let (tx, rx) = oneshot::channel::<WebSocket>();
    let grant = UpgradeGrant {
        token_id: principal.token_id,
        token_sha256: principal.token_sha256,
        kind: principal.kind,
        thread: principal.thread,
        purpose,
    };
    let shutdown = shared.shutdown_signal();
    let hand_over = shared.cfg.short_deadline;
    let s = shared.clone();
    let task = async move {
        let _slot = slot;
        if admitted.await.is_err() {
            return;
        }
        #[cfg(test)]
        s.faults.step("hand-over bound started");
        let Ok(Ok(socket)) = hand_over_bound(hand_over, rx).await else {
            return;
        };
        handler
            .run(UpgradedSocket {
                socket,
                grant,
                shutdown,
                shared: s,
            })
            .await;
    };
    #[cfg(test)]
    let task = shared.faults.signal_first_park(task);
    if shared.registry.spawn(TaskKind::Socket, task).is_err() {
        return Err(ApiError::shutting_down());
    }
    // 4. The final admission read: this snapshot decides the token's validity.
    verify_until(&shared, deadline, principal.token_sha256).await?;
    #[cfg(test)]
    shared.faults.before_ticket_removal().await;
    // 5. The conditional removal decides single use.
    if shared
        .tickets
        .remove_if_valid(&ticket_sha, shared.cfg.clock.now())
        .is_none()
    {
        return Err(ApiError::unauthorized());
    }
    #[cfg(test)]
    shared.faults.step("ticket removed");
    // 6. `101`; axum's upgrade task hands the socket to the registered task.
    let _ = admitted_tx.send(());
    Ok(ws.on_upgrade(move |socket| async move {
        let _ = tx.send(socket);
    }))
}

/// The socket task's wait for its socket, bounded by the short deadline from the moment the
/// ticket is removed; a socket queued before the bound is served (design §2.4).
#[allow(
    clippy::disallowed_methods,
    reason = "the hand-over bound, polled socket first (http-surface §2.4)"
)]
fn hand_over_bound<F: Future>(within: std::time::Duration, rx: F) -> tokio::time::Timeout<F> {
    tokio::time::timeout(within, rx)
}
