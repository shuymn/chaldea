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
mod ctx;
mod error;
mod guard;
mod listener;
mod registry;
mod routes;
mod tickets;
mod trace;
mod upgrade;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::store::Store;

pub use auth::{
    AuthFailure, Authorized, MintedToken, ObjectScope, Principal, Secret, TokenService, deny_for,
    verify_hash_in, verify_secret_in,
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

/// The server's monotonic clock (design §2.3): request origins, deadlines, the cutoff
/// checks, and ticket expiry are all read on it; tests inject one.
pub trait MonotonicClock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// [`Instant::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemMonotonic;

impl MonotonicClock for SystemMonotonic {
    fn now(&self) -> Instant {
        Instant::now()
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
    /// A WebSocket ticket's lifetime.
    pub ticket_ttl: Duration,
    /// The clock every request instant and deadline is read on.
    pub clock: Arc<dyn MonotonicClock>,
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
            ticket_ttl: Duration::from_secs(30),
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
    pub(crate) sink: Arc<dyn LifecycleSink>,
    pub(crate) tickets: tickets::TicketStore,
    /// Bumped after each committed revocation; sockets re-check their token (PR 7b).
    pub(crate) revocations: watch::Sender<u64>,
    /// Set once by shutdown; every registered task watches it.
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) faults: Faults,
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
    /// reaches `at`.
    pub(crate) fn real(&self, at: Instant) -> Instant {
        Instant::now() + at.saturating_duration_since(self.cfg.clock.now())
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
    /// Builds the server and its router from `mounts`.
    ///
    /// # Panics
    ///
    /// When `mounts` does not mount every route of [`ROUTES`] exactly once (design §2.2).
    #[must_use]
    pub fn new(
        cfg: HttpConfig,
        store: Store,
        sink: Arc<dyn LifecycleSink>,
        mounts: Mounts,
    ) -> HttpServer {
        let shared = Arc::new(Shared {
            store,
            cfg,
            registry: TaskRegistry::new(),
            sink,
            tickets: tickets::TicketStore::default(),
            revocations: watch::channel(0).0,
            shutdown: watch::channel(false).0,
            faults: Faults::default(),
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
    /// Tasks that ended before the deadline.
    pub exited: KindCounts,
    /// Tasks still running at the deadline, which got an abort request; a request does not
    /// prove the task has ended.
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

/// Test-only fault points (design §7); inert in other builds.
#[derive(Default)]
pub(crate) struct Faults {
    #[cfg(test)]
    pub(crate) inner: tests::faults::Points,
}

impl Faults {
    /// Counts one poll of a request body by the application.
    pub(crate) fn body_polled(&self) {
        #[cfg(test)]
        self.inner.body_polled();
    }

    /// Runs on the blocking thread before every store read of a request or socket.
    pub(crate) fn before_read(&self) {
        #[cfg(test)]
        self.inner.before_read();
    }

    /// Runs on the blocking thread after every store read of a request or socket.
    pub(crate) fn after_read(&self) {
        #[cfg(test)]
        self.inner.after_read();
    }

    /// Counts one write job submitted to the store.
    pub(crate) fn write_submitted(&self) {
        #[cfg(test)]
        self.inner.write_submitted();
    }

    /// Runs in an upgrade admission between the final read and the ticket removal.
    pub(crate) async fn before_ticket_removal(&self) {
        #[cfg(test)]
        self.inner.before_ticket_removal().await;
    }

    /// An injected accept error, taken before each real accept.
    pub(crate) fn accept_error(&self) -> Option<std::io::Error> {
        #[cfg(test)]
        return self.inner.accept_error();
        #[cfg(not(test))]
        None
    }
}
