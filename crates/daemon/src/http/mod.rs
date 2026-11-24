//! The HTTP server (`docs/design/http-surface.md`, cited as "design §N"): the accept loop
//! and connections, the outer layer and the per-route guard, tokens, the store access
//! contexts, the status mapping, and the task registry that owns everything outliving a
//! request.
//!
//! The rules (design §2.1):
//!
//! 1. **The platform decides framing; one guard decides access.** hyper parses requests and
//!    bounds heads; every access decision before a handler runs is the outer layer plus one
//!    guard per route built from that route's [`RouteSpec`], and the router is built only
//!    from [`ROUTES`].
//! 2. **The database decides.** Tokens are checked by hash in the transaction that uses
//!    them; tickets live in memory, single use.
//! 3. **One task registry owns everything that outlives a request** ([`TaskRegistry`]),
//!    with one closing transition shared with shutdown.
//! 4. **Every wait has a deadline**: a request's deadline starts at its connection's first
//!    head byte, and each connection carries one request.

mod auth;
mod blobs;
mod commands;
mod ctx;
mod error;
mod guard;
mod listener;
mod pair;
mod registry;
mod routes;
mod scope;
mod sync_api;
mod tickets;
mod trace;
mod upgrade;
mod ws;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::store::Store;

pub use auth::{
    AuthFailure, Authorized, MintedToken, ObjectScope, Principal, Secret, TokenService, deny_for,
    verify_hash_in, verify_secret_in,
};
pub use commands::{
    CommandCtx, CommandEffect, CommandHandler, CommandRegistry, EffectRun, Intent, IntentFn,
    Prepared,
};
pub use ctx::{Deadline, ReadCtx, RequestStart, WriteCtx};
pub use error::ApiError;
pub use registry::{Closing, KindCounts, TaskKind, TaskRegistry};
pub use routes::{
    ActionRule, BodyRule, Credential, Mount, MountKind, Mounts, OriginRule, ROUTES, ReadHandler,
    RouteSpec, TicketPurposeKind, UpgradeHandler, WriteHandler, spec,
};
pub use tickets::TicketPurpose;
pub use trace::redact_secrets;
pub use upgrade::{ShutdownSignal, UpgradeGrant, UpgradedSocket};

pub(crate) use auth::{TOKEN_PREFIX, has_secret_form, verify_in_read};
pub(crate) use ctx::until_deadline;
pub(crate) use error::refused;

/// The server's monotonic clock (design §2.3): request origins, deadlines, the cutoff
/// checks, and ticket expiry are all read on it; tests inject one.
pub trait MonotonicClock: Send + Sync + 'static {
    fn now(&self) -> Instant;

    /// The real instant (`Instant` time, for timers and the store's waits) at which this
    /// clock reads `at`: a function of the clock's state, never a second reading of either
    /// clock, so a thread preempted between two readings cannot move a deadline.
    fn real(&self, at: Instant) -> Instant;
}

/// [`Instant::now`]: its instants are real ones.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemMonotonic;

impl MonotonicClock for SystemMonotonic {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn real(&self, at: Instant) -> Instant {
        at
    }
}

/// The server's settings. [`HttpConfig::new`] gives the daemon's defaults.
#[derive(Clone)]
pub struct HttpConfig {
    /// The bound port; `Host` must name it.
    pub port: u16,
    /// Every request without its own budget, from its connection's first head byte.
    pub short_deadline: Duration,
    /// How long past the deadline a submitted write waits for the writer's reply.
    pub write_reply_grace: Duration,
    /// A head not completed within it ends the connection (EOF).
    pub header_read_timeout: Duration,
    /// A write pending this long without a byte accepted ends the connection.
    pub write_stall_timeout: Duration,
    /// A larger head is answered 431 by hyper.
    pub max_header_bytes: usize,
    /// A larger declared request body is 413.
    pub max_body_bytes: u64,
    /// Connections served at once; more wait in the kernel backlog.
    pub max_connections: usize,
    /// Blob bodies held by responses at once; more are 503 `busy`.
    pub blob_slots: usize,
    /// The WebSocket settings and caps.
    pub ws: WsConfig,
    /// A WebSocket ticket's lifetime.
    pub ticket_ttl: Duration,
    /// A pair code's lifetime.
    pub pair_code_ttl: Duration,
    /// The clock every request instant and deadline is read on.
    pub clock: Arc<dyn MonotonicClock>,
}

/// The WebSocket settings (design §2.6).
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// A heads refresh becomes due this often.
    pub heads_interval: Duration,
    /// One frame not sent within it drops the socket.
    pub send_timeout: Duration,
    /// No inbound frame (the client answers pings) within it drops the socket.
    pub idle_timeout: Duration,
    /// The largest inbound message and frame; larger closes the socket (1009).
    pub max_message_bytes: usize,
    /// The socket's write buffer bound.
    pub max_write_buffer_bytes: usize,
    /// Streams one `subscribe` may name; more closes the socket (1008).
    pub max_subscriptions: usize,
    /// Upgraded sockets at once; more are 503 `busy`.
    pub max_sockets: usize,
    /// Upgraded sockets at once for one token.
    pub max_sockets_per_token: usize,
    /// How long a close frame may take to send, best effort.
    pub close_grace: Duration,
}

impl Default for WsConfig {
    fn default() -> Self {
        WsConfig {
            heads_interval: Duration::from_secs(30),
            send_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(75),
            max_message_bytes: 64 * 1024,
            max_write_buffer_bytes: 256 * 1024,
            max_subscriptions: 1024,
            max_sockets: 128,
            max_sockets_per_token: 16,
            close_grace: Duration::from_secs(1),
        }
    }
}

impl HttpConfig {
    /// The daemon's defaults (design §2.2) for `port`.
    #[must_use]
    pub fn new(port: u16) -> HttpConfig {
        HttpConfig {
            port,
            short_deadline: Duration::from_secs(5),
            write_reply_grace: Duration::from_millis(100),
            header_read_timeout: Duration::from_secs(5),
            write_stall_timeout: Duration::from_secs(30),
            max_header_bytes: 8 * 1024,
            max_body_bytes: 4 * 1024 * 1024,
            max_connections: 128,
            blob_slots: 4,
            ws: WsConfig::default(),
            ticket_ttl: Duration::from_secs(30),
            pair_code_ttl: Duration::from_secs(60),
            clock: Arc::new(SystemMonotonic),
        }
    }
}

/// The headers a request may carry at most; more are answered 431 by hyper.
pub const MAX_HEADERS: usize = 64;

/// PR 8's seam: told of every fatal store outcome a write meets while serving.
pub trait LifecycleSink: Send + Sync + 'static {
    fn store_fatal(&self, f: StoreFatal);
}

/// A fatal store outcome (design §2.2): each stops the daemon (PR 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreFatal {
    CommitUnknown,
    WriterGone,
    Failed,
    /// A command's receipt could not be completed after its effect ran (PR 7c's runner).
    ReceiptUncompleted {
        command_id: chaldea_core::ids::CommandId,
    },
}

/// State every request, task, and handle of one server shares.
pub(crate) struct Shared {
    pub(crate) store: Store,
    pub(crate) cfg: HttpConfig,
    pub(crate) registry: TaskRegistry,
    /// The command handlers (design §2.8).
    pub(crate) commands: commands::CommandRegistry,
    pub(crate) sink: Arc<dyn LifecycleSink>,
    pub(crate) tickets: tickets::TicketStore,
    pub(crate) pair_codes: tickets::PairCodes,
    /// Blob bodies held by responses (design §2.7).
    pub(crate) blob_slots: Arc<tokio::sync::Semaphore>,
    /// Upgraded sockets, in all and per token (design §2.4 step 3).
    pub(crate) sockets: upgrade::SocketSlots,
    /// Bumped after each committed revocation; every socket re-checks its token.
    pub(crate) revocations: watch::Sender<u64>,
    /// Set once by shutdown; every registered task watches it.
    pub(crate) shutdown: watch::Sender<bool>,
    /// Test-only fault points (design §7); absent from other builds.
    #[cfg(test)]
    pub(crate) faults: tests::faults::Points,
}

impl Shared {
    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal::new(self.shutdown.subscribe())
    }

    /// Whether the server's clock has reached `at`.
    pub(crate) fn passed(&self, at: Instant) -> bool {
        self.cfg.clock.now() >= at
    }

    /// The real instant (for timers and the store's waits) at which the server's clock
    /// reaches `at`; `at` itself with the system clock.
    pub(crate) fn real(&self, at: Instant) -> Instant {
        self.cfg.clock.real(at)
    }

    /// [`Shared::real`] as a tokio instant.
    pub(crate) fn timer(&self, at: Instant) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.real(at))
    }

    pub(crate) fn nudge(&self) {
        self.revocations.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// The HTTP server. Cheap to keep; [`HttpServer::serve`] starts it on a listener.
pub struct HttpServer {
    shared: Arc<Shared>,
    tokens: TokenService,
    router: axum::Router,
}

impl HttpServer {
    /// Builds the server and its router from `mounts`; `commands` holds the command
    /// handlers ([`CommandRegistry::mvp`] in the daemon).
    ///
    /// # Panics
    ///
    /// When `mounts` does not mount every route of [`ROUTES`] exactly once (design §2.2).
    #[must_use]
    pub fn new(
        cfg: HttpConfig,
        store: Store,
        commands: CommandRegistry,
        sink: Arc<dyn LifecycleSink>,
        mounts: Mounts,
    ) -> HttpServer {
        let shared = Arc::new(Shared {
            store,
            registry: TaskRegistry::new(),
            commands,
            sink,
            tickets: tickets::TicketStore::default(),
            pair_codes: tickets::PairCodes::default(),
            blob_slots: Arc::new(tokio::sync::Semaphore::new(cfg.blob_slots)),
            sockets: upgrade::SocketSlots::default(),
            cfg,
            revocations: watch::channel(0).0,
            shutdown: watch::channel(false).0,
            #[cfg(test)]
            faults: tests::faults::Points::default(),
        });
        let router = routes::build_router(mounts, &shared);
        HttpServer {
            tokens: TokenService::new(shared.clone()),
            shared,
            router,
        }
    }

    /// Serves `listener` (PR 8's, bound to `127.0.0.1`) until [`ServeHandle::shutdown`].
    /// Must be called inside a tokio runtime. The listener belongs to the accept task.
    #[must_use]
    pub fn serve(&self, listener: TcpListener) -> ServeHandle {
        let accept = listener::accept_loop(self.shared.clone(), listener, self.router.clone());
        // Refused only once the registry is closing: the listener is dropped with it.
        let _ = self.shared.registry.spawn(TaskKind::Accept, accept);
        ServeHandle {
            shared: self.shared.clone(),
        }
    }

    /// A handle that drains the registry without serving: for a server whose startup ended
    /// before `serve` but may have registered work (a token write's observer).
    #[must_use]
    pub fn shutdown_handle(&self) -> ServeHandle {
        ServeHandle {
            shared: self.shared.clone(),
        }
    }

    /// Token issuance, verification, and revocation.
    #[must_use]
    pub fn tokens(&self) -> &TokenService {
        &self.tokens
    }

    /// Registered tasks that have not ended (accept loop, connections, sockets, writes).
    #[must_use]
    pub fn tasks(&self) -> usize {
        self.shared.registry.len()
    }

    /// Asks every WebSocket to re-check its token now.
    pub fn nudge_revocation(&self) {
        self.shared.nudge();
    }

    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// Stops a serving [`HttpServer`].
pub struct ServeHandle {
    shared: Arc<Shared>,
}

/// What [`ServeHandle::shutdown`] observed, per task kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpShutdownReport {
    /// Tasks registered when shutdown began that were gone from the registry at the abort
    /// pass. That pass runs once the registry is empty or the deadline has passed, so a task
    /// that ends after the deadline but before the pass counts here.
    pub exited: KindCounts,
    /// Tasks still in the registry at the abort pass (once the registry is empty or the
    /// deadline has passed), which got an abort request; a request does not prove the task
    /// has ended.
    pub abort_requested: KindCounts,
}

impl ServeHandle {
    /// Closes the registry, stops accepting, signals every task, waits until `deadline` for
    /// them to end, requests abort of the rest, and reports (design §2.2 "Shutdown").
    pub async fn shutdown(self, deadline: Instant) -> HttpShutdownReport {
        let registry = &self.shared.registry;
        let snapshot = registry.close();
        self.shared.shutdown.send_replace(true);
        registry.wait_empty_until(deadline).await;
        let abort_requested = registry.abort_remaining();
        HttpShutdownReport {
            exited: snapshot.minus(&abort_requested),
            abort_requested,
        }
    }
}
