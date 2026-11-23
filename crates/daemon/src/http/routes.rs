//! The route table, mounts, and the router built only from them (design §2.2 "Route
//! table", "Building the router").

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request};
use axum::response::Response;
use chaldea_core::auth::{Action, TokenKind};
use futures_util::future::BoxFuture;

use super::Shared;
use super::ctx::{ReadCtx, WriteCtx};
use super::error::ApiError;
use super::guard::{GuardLayer, fallback};
use super::upgrade::UpgradedSocket;

/// What a route's handler may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountKind {
    /// Gets [`ReadCtx`] only; wrapped in a timeout at the deadline (408).
    Read,
    /// Gets [`WriteCtx`]; never wrapped in a dropping timeout.
    Write,
    /// Gets an admitted upgrade.
    Upgrade,
}

/// The route's `Origin` rule (mvp §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRule {
    /// Absent, or exactly `http://` + the received `Host`.
    AbsentOrMatching,
    /// Present and matching.
    PresentMatching,
    /// Absent.
    MustBeAbsent,
    /// Not checked (the static PWA).
    NotChecked,
}

/// Which ticket purpose a ticket route accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketPurposeKind {
    Events,
    /// For the path's `{thread}`.
    Attach,
}

/// The route's credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    None,
    /// A Bearer token of one of these kinds.
    Bearer(&'static [TokenKind]),
    /// A single-use ticket in the `ticket` query parameter.
    Ticket(TicketPurposeKind),
    /// A pair code in the body (checked by the handler).
    PairCode,
}

/// The route's action check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRule {
    None,
    Fixed(Action),
    /// `{name}` must be in `core::auth::MVP_COMMANDS`; its action is checked.
    ByCommandName,
}

/// The route's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRule {
    NoBody,
    Json,
}

/// One route: everything the guard decides before the handler.
#[derive(Debug)]
pub struct RouteSpec {
    pub method: Method,
    /// axum path syntax, e.g. `/api/blobs/{sha256}`.
    pub path: &'static str,
    pub mount: MountKind,
    pub origin: OriginRule,
    pub credential: Credential,
    pub action: ActionRule,
    pub body: BodyRule,
    /// The PR that implements the handler; until then 501.
    pub owner_pr: u8,
}

impl RouteSpec {
    /// The static PWA's routes, the only ones whose handlers set their own caching
    /// (mvp §2.5; PR 18).
    #[must_use]
    pub fn is_pwa(&self) -> bool {
        matches!(self.path, "/" | "/assets/{*path}")
    }
}

const OWNER: &[TokenKind] = &[TokenKind::Owner];
const READERS: &[TokenKind] = &[TokenKind::Owner, TokenKind::Agent, TokenKind::Rescue];
const OWNER_RESCUE: &[TokenKind] = &[TokenKind::Owner, TokenKind::Rescue];
const OWNER_AGENT: &[TokenKind] = &[TokenKind::Owner, TokenKind::Agent];
const HOOK: &[TokenKind] = &[TokenKind::Hook];
const POSTERS: &[TokenKind] = &[TokenKind::Owner, TokenKind::Rescue, TokenKind::Extension];

const fn get(
    path: &'static str,
    mount: MountKind,
    origin: OriginRule,
    credential: Credential,
    action: ActionRule,
    owner_pr: u8,
) -> RouteSpec {
    RouteSpec {
        method: Method::GET,
        path,
        mount,
        origin,
        credential,
        action,
        body: BodyRule::NoBody,
        owner_pr,
    }
}

const fn post(
    path: &'static str,
    mount: MountKind,
    origin: OriginRule,
    credential: Credential,
    action: ActionRule,
    owner_pr: u8,
) -> RouteSpec {
    RouteSpec {
        method: Method::POST,
        path,
        mount,
        origin,
        credential,
        action,
        body: BodyRule::Json,
        owner_pr,
    }
}

/// The one route table: the production source for the router and the guard (mvp §2.5).
pub static ROUTES: &[RouteSpec] = {
    use ActionRule as A;
    use Credential as C;
    use MountKind as M;
    use OriginRule as O;
    use TicketPurposeKind as T;
    &[
        get("/", M::Read, O::NotChecked, C::None, A::None, 18),
        get(
            "/assets/{*path}",
            M::Read,
            O::NotChecked,
            C::None,
            A::None,
            18,
        ),
        post(
            "/api/pair",
            M::Write,
            O::PresentMatching,
            C::PairCode,
            A::None,
            7,
        ),
        post(
            "/api/pair-codes",
            M::Read,
            O::MustBeAbsent,
            C::Bearer(OWNER),
            A::None,
            7,
        ),
        post(
            "/api/ws-ticket",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadStream),
            7,
        ),
        get(
            "/api/ws",
            M::Upgrade,
            O::AbsentOrMatching,
            C::Ticket(T::Events),
            A::None,
            7,
        ),
        get(
            "/api/attach/{thread}",
            M::Upgrade,
            O::AbsentOrMatching,
            C::Ticket(T::Attach),
            A::Fixed(Action::Attach),
            11,
        ),
        get(
            "/api/events",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadStream),
            7,
        ),
        get(
            "/api/blobs/{sha256}",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadBlob),
            7,
        ),
        get(
            "/api/threads",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadThreadList),
            7,
        ),
        get(
            "/api/worktrees",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadWorktrees),
            7,
        ),
        get(
            "/api/adapters",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(READERS),
            A::Fixed(Action::ReadAdapters),
            10,
        ),
        get(
            "/api/doctor",
            M::Read,
            O::AbsentOrMatching,
            C::Bearer(OWNER_RESCUE),
            A::Fixed(Action::ReadDoctor),
            21,
        ),
        post(
            "/api/commands/{name}",
            M::Write,
            O::AbsentOrMatching,
            C::Bearer(OWNER_AGENT),
            A::ByCommandName,
            7,
        ),
        post(
            "/hook",
            M::Write,
            O::MustBeAbsent,
            C::Bearer(HOOK),
            A::None,
            9,
        ),
        post(
            "/events",
            M::Write,
            O::MustBeAbsent,
            C::Bearer(POSTERS),
            A::Fixed(Action::PostEvents),
            20,
        ),
    ]
};

/// The route of `ROUTES` with this method and path template.
#[must_use]
pub fn spec(method: &Method, path: &str) -> Option<&'static RouteSpec> {
    ROUTES
        .iter()
        .find(|s| s.method == *method && s.path == path)
}

/// A read mount's handler.
pub trait ReadHandler: Send + Sync + 'static {
    fn call(
        &self,
        ctx: ReadCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response, ApiError>>;
}

impl<F, Fut> ReadHandler for F
where
    F: Fn(ReadCtx, Request<Body>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, ApiError>> + Send + 'static,
{
    fn call(
        &self,
        ctx: ReadCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response, ApiError>> {
        Box::pin(self(ctx, req))
    }
}

/// A write mount's handler.
pub trait WriteHandler: Send + Sync + 'static {
    fn call(
        &self,
        ctx: WriteCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response, ApiError>>;
}

impl<F, Fut> WriteHandler for F
where
    F: Fn(WriteCtx, Request<Body>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, ApiError>> + Send + 'static,
{
    fn call(
        &self,
        ctx: WriteCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response, ApiError>> {
        Box::pin(self(ctx, req))
    }
}

/// An upgrade mount's handler: runs in the registered socket task with the admitted socket.
pub trait UpgradeHandler: Send + Sync + 'static {
    fn run(&self, socket: UpgradedSocket) -> BoxFuture<'static, ()>;
}

impl<F, Fut> UpgradeHandler for F
where
    F: Fn(UpgradedSocket) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn run(&self, socket: UpgradedSocket) -> BoxFuture<'static, ()> {
        Box::pin(self(socket))
    }
}

#[derive(Clone)]
pub(crate) enum Endpoint {
    /// The handler's PR has not merged: the guard answers 501.
    Unimplemented,
    Read(Arc<dyn ReadHandler>),
    Write(Arc<dyn WriteHandler>),
    Upgrade(Arc<dyn UpgradeHandler>),
}

/// A route's handler, built only through the typed constructors.
#[derive(Clone)]
pub struct Mount {
    spec: &'static RouteSpec,
    endpoint: Endpoint,
}

impl Mount {
    fn check(spec: &'static RouteSpec, kind: MountKind) {
        assert!(
            spec.mount == kind,
            "{} {} is a {:?} route, mounted as {kind:?}",
            spec.method,
            spec.path,
            spec.mount
        );
    }

    /// # Panics
    ///
    /// When `spec` is not a read route.
    pub fn read(spec: &'static RouteSpec, h: impl ReadHandler) -> Mount {
        Self::check(spec, MountKind::Read);
        Mount {
            spec,
            endpoint: Endpoint::Read(Arc::new(h)),
        }
    }

    /// # Panics
    ///
    /// When `spec` is not a write route.
    pub fn write(spec: &'static RouteSpec, h: impl WriteHandler) -> Mount {
        Self::check(spec, MountKind::Write);
        Mount {
            spec,
            endpoint: Endpoint::Write(Arc::new(h)),
        }
    }

    /// # Panics
    ///
    /// When `spec` is not an upgrade route.
    pub fn upgrade(spec: &'static RouteSpec, h: impl UpgradeHandler) -> Mount {
        Self::check(spec, MountKind::Upgrade);
        Mount {
            spec,
            endpoint: Endpoint::Upgrade(Arc::new(h)),
        }
    }

    /// The route with no handler yet: the guard answers 501 after its checks.
    #[must_use]
    pub fn unimplemented(spec: &'static RouteSpec) -> Mount {
        Mount {
            spec,
            endpoint: Endpoint::Unimplemented,
        }
    }

    #[must_use]
    pub fn spec(&self) -> &'static RouteSpec {
        self.spec
    }
}

/// The mounts a server is built from.
#[derive(Clone, Default)]
pub struct Mounts(Vec<Mount>);

impl Mounts {
    /// Every route of [`ROUTES`]: PR 7's sync reads, blobs, tickets, pairing, and the
    /// WebSocket loop, and a 501 handler for every route whose PR has not mounted one.
    ///
    /// # Panics
    ///
    /// Never: every route named here is in [`ROUTES`].
    #[must_use]
    pub fn mvp() -> Mounts {
        use super::{blobs, pair, sync_api, ws};
        let at = |method: Method, path: &str| {
            spec(&method, path).unwrap_or_else(|| panic!("{method} {path} is in ROUTES"))
        };
        Mounts(ROUTES.iter().map(Mount::unimplemented).collect())
            .with(Mount::read(
                at(Method::GET, "/api/events"),
                sync_api::events,
            ))
            .with(Mount::read(
                at(Method::GET, "/api/threads"),
                sync_api::threads,
            ))
            .with(Mount::read(
                at(Method::GET, "/api/worktrees"),
                sync_api::worktrees,
            ))
            .with(Mount::read(
                at(Method::GET, "/api/blobs/{sha256}"),
                blobs::download,
            ))
            .with(Mount::read(
                at(Method::POST, "/api/ws-ticket"),
                ws::ws_ticket,
            ))
            .with(Mount::upgrade(
                at(Method::GET, "/api/ws"),
                ws::events_socket,
            ))
            .with(Mount::read(
                at(Method::POST, "/api/pair-codes"),
                pair::pair_codes,
            ))
            .with(Mount::write(at(Method::POST, "/api/pair"), pair::pair))
    }

    /// No mounts (for building inconsistent sets in tests).
    #[must_use]
    pub fn empty() -> Mounts {
        Mounts(Vec::new())
    }

    /// Replaces the mount of `m`'s route, or adds `m`.
    #[must_use]
    pub fn with(mut self, m: Mount) -> Mounts {
        match self.0.iter_mut().find(|x| std::ptr::eq(x.spec, m.spec)) {
            Some(x) => *x = m,
            None => self.0.push(m),
        }
        self
    }

    /// Adds `m` even if its route is mounted already (for tests of the refusal).
    #[must_use]
    pub fn push(mut self, m: Mount) -> Mounts {
        self.0.push(m);
        self
    }
}

/// The only `Router::route_service` call: each mount under its spec's path, behind the
/// route's guard, plus the fallback for unknown paths.
///
/// # Panics
///
/// When a mount's spec is not in `ROUTES`, a route is mounted twice, or a `ROUTES` entry
/// has no mount (design §2.2).
#[allow(
    clippy::disallowed_methods,
    reason = "the one place the router is built (design §2.2)"
)]
pub(crate) fn build_router(mounts: Mounts, shared: &Arc<Shared>) -> Router {
    let mut mounts = mounts.0;
    for m in &mounts {
        assert!(
            ROUTES.iter().any(|s| std::ptr::eq(s, m.spec)),
            "a mount for {} {} whose spec is not in ROUTES",
            m.spec.method,
            m.spec.path
        );
    }
    let mut router = Router::new();
    for s in ROUTES {
        let i = mounts
            .iter()
            .position(|m| std::ptr::eq(m.spec, s))
            .unwrap_or_else(|| panic!("{} {} has no mount", s.method, s.path));
        let m = mounts.swap_remove(i);
        assert!(
            !mounts.iter().any(|m| std::ptr::eq(m.spec, s)),
            "{} {} is mounted twice",
            s.method,
            s.path
        );
        router = router.route_service(
            s.path,
            GuardLayer::new(s, shared.clone()).into_service(m.endpoint),
        );
    }
    router.fallback(fallback)
}
