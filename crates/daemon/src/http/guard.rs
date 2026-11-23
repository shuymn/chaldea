//! The outer layer and the per-route guard (design §2.2 "Guard order").
//!
//! `OuterService` runs before routing: the request span, the request origin, the body
//! wrapper, the `Host` check, and on the way out `Connection` and `Cache-Control`. The
//! guard runs per route, built from its [`RouteSpec`]: method → `Origin` → credential →
//! kind and action → framing → size → unimplemented → deadline. No application code polls
//! or uses a body byte before the guard accepts the request.

use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use chaldea_core::auth::grant;
use futures_util::future::BoxFuture;
use http_body_util::Limited;
use hyper::body::Incoming;
use tower::{Layer, Service, ServiceExt};
use tower_http::sensitive_headers::{SetSensitiveRequestHeaders, SetSensitiveRequestHeadersLayer};
use tracing::{Instrument, Span};

use super::Shared;
use super::auth::{Principal, TICKET_PREFIX, has_secret_form, parse_bearer, sha256};
use super::commands;
use super::ctx::{Deadline, ReadCtx, RequestStart, WriteCtx, until_deadline, verify_until};
use super::error::ApiError;
use super::routes::{ActionRule, Credential, Endpoint, OriginRule, RouteSpec, TicketPurposeKind};
use super::tickets::{TicketEntry, TicketPurpose};
use super::trace::{UNMATCHED, request_span};
use super::upgrade;

/// In tests, a request body that counts the application's polls.
#[cfg(test)]
struct AppBody {
    inner: Incoming,
    shared: Arc<Shared>,
}

#[cfg(test)]
impl hyper::body::Body for AppBody {
    type Data = axum::body::Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, hyper::Error>>> {
        self.shared.faults.body_polled();
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// The per-connection outer service (design §2.2 step 1), behind the sensitive-header
/// marking so no layer sees a credential header unmarked.
#[derive(Clone)]
pub(crate) struct OuterService {
    shared: Arc<Shared>,
    router: Router,
    first_byte: Arc<OnceLock<Instant>>,
}

impl OuterService {
    pub(crate) fn new(
        shared: Arc<Shared>,
        router: Router,
        first_byte: Arc<OnceLock<Instant>>,
    ) -> SetSensitiveRequestHeaders<OuterService> {
        SetSensitiveRequestHeadersLayer::new([
            header::AUTHORIZATION,
            header::COOKIE,
            header::SEC_WEBSOCKET_KEY,
        ])
        .layer(OuterService {
            shared,
            router,
            first_byte,
        })
    }
}

impl Service<Request<Incoming>> for OuterService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let span = request_span(req.method());
        let shared = self.shared.clone();
        let router = self.router.clone();
        let start = RequestStart(
            self.first_byte
                .get()
                .copied()
                .unwrap_or_else(|| shared.cfg.clock.now()),
        );
        let fut = async move {
            let started = Instant::now();
            let (mut parts, body) = req.into_parts();
            parts.extensions.insert(start);
            #[cfg(test)]
            let body = AppBody {
                inner: body,
                shared: shared.clone(),
            };
            let body = Body::new(body);
            let resp = match check_host(&parts.headers, &parts.uri, shared.cfg.port) {
                Err(e) => {
                    // Rejected before routing: no route template applies.
                    Span::current().record("route", tracing::field::display(UNMATCHED));
                    e.into_response()
                }
                Ok(()) => match router.oneshot(Request::from_parts(parts, body)).await {
                    Ok(r) => r,
                    Err(never) => match never {},
                },
            };
            Ok(finish(resp, started))
        };
        Box::pin(fut.instrument(span))
    }
}

/// The response's connection and caching headers, and the request record.
fn finish(mut resp: Response<Body>, started: Instant) -> Response<Body> {
    let status = resp.status();
    let h = resp.headers_mut();
    // One request per connection: every final response but a successful 101 is the
    // connection's last message; a 101 keeps exactly `Connection: Upgrade` (design §2.2).
    if status == StatusCode::SWITCHING_PROTOCOLS {
        h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    } else {
        h.insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    h.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    let span = Span::current();
    span.record("status", status.as_u16());
    span.record(
        "latency_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    tracing::info!("request finished");
    resp
}

/// Whether `v` is `127.0.0.1:<port>` or `localhost:<port>` (`localhost` ASCII
/// case-insensitive).
fn is_own_authority(v: &str, port: u16) -> bool {
    let suffix = format!(":{port}");
    let Some(host) = v.strip_suffix(suffix.as_str()) else {
        return false;
    };
    host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost")
}

/// Exactly one `Host`, naming the daemon; an absolute-form target carries exactly that
/// authority (`localhost` compared ASCII case-insensitively; no userinfo). Else 421.
fn check_host(headers: &HeaderMap, uri: &axum::http::Uri, port: u16) -> Result<(), ApiError> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return Err(ApiError::misdirected_request());
    };
    let ok = host.to_str().is_ok_and(|h| {
        is_own_authority(h, port)
            && uri
                .authority()
                .is_none_or(|a| a.as_str().eq_ignore_ascii_case(h))
    });
    if ok {
        Ok(())
    } else {
        Err(ApiError::misdirected_request())
    }
}

/// The router's fallback for unknown paths.
pub(crate) async fn fallback() -> Response<Body> {
    Span::current().record("route", tracing::field::display(UNMATCHED));
    ApiError::not_found().into_response()
}

/// Builds one route's guard.
pub(crate) struct GuardLayer {
    spec: &'static RouteSpec,
    shared: Arc<Shared>,
}

impl GuardLayer {
    pub(crate) fn new(spec: &'static RouteSpec, shared: Arc<Shared>) -> Self {
        GuardLayer { spec, shared }
    }

    pub(crate) fn into_service(self, endpoint: Endpoint) -> GuardService {
        GuardService {
            spec: self.spec,
            shared: self.shared,
            endpoint,
        }
    }
}

/// One route's guard and its endpoint.
#[derive(Clone)]
pub(crate) struct GuardService {
    spec: &'static RouteSpec,
    shared: Arc<Shared>,
    endpoint: Endpoint,
}

impl Service<Request<Body>> for GuardService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response<Body>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            Span::current().record("route", tracing::field::display(this.spec.path));
            let mut resp = this
                .handle(req)
                .await
                .unwrap_or_else(IntoResponse::into_response);
            // Only the PWA's routes keep their own caching (mvp §2.5).
            if !this.spec.is_pwa() {
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            Ok(resp)
        })
    }
}

/// What the credential step admitted.
struct Admitted {
    principal: Option<Principal>,
    ticket: Option<([u8; 32], TicketEntry)>,
}

impl GuardService {
    async fn handle(&self, req: Request<Body>) -> Result<Response<Body>, ApiError> {
        let spec = self.spec;
        let shared = &self.shared;
        // The body stays untouched, apart from the head, until every check has passed.
        let (mut parts, body) = req.into_parts();
        let start = parts
            .extensions
            .get::<RequestStart>()
            .map_or_else(|| shared.cfg.clock.now(), |s| s.0);
        // The route's budget: a command states its own, every other route has the short one.
        let budget = match spec.action {
            ActionRule::ByCommandName => commands::budget(shared, parts.uri.path()),
            _ => shared.cfg.short_deadline,
        };
        let deadline = start + budget;

        // 2. Method: exactly the spec's (`HEAD` is not accepted on `GET` routes).
        if parts.method != spec.method {
            return Err(ApiError::method_not_allowed(spec.method.clone()));
        }
        // 3. Origin.
        check_origin(spec.origin, &parts.headers)?;
        // 4. Credential.
        let admitted = self.credential(&parts, deadline).await?;
        // 5. Kind and action.
        if let Some(p) = &admitted.principal {
            self.kind_and_action(p, parts.uri.path())?;
        }
        // 6. Framing, 7. size.
        check_framing(&parts.method, &parts.headers, shared.cfg.max_body_bytes)?;
        let uri_path = parts.uri.path().to_owned();
        // The handler's request: the deadline in its extensions, the body bounded.
        let request = move || {
            parts.extensions.insert(Deadline(deadline));
            let max = usize::try_from(shared.cfg.max_body_bytes).unwrap_or(usize::MAX);
            Request::from_parts(parts, Body::new(Limited::new(body, max)))
        };
        // 8. Unimplemented: the route's handler, or the command's.
        let command_unimplemented = spec.action == ActionRule::ByCommandName
            && path_param(spec.path, &uri_path, "name")
                .is_none_or(|name| shared.commands.handler(name).is_none());
        match &self.endpoint {
            Endpoint::Unimplemented => Err(ApiError::not_implemented()),
            _ if command_unimplemented => Err(ApiError::not_implemented()),
            // 9. Deadline: no handler is invoked once it has passed.
            _ if shared.passed(deadline) => Err(ApiError::request_timeout()),
            Endpoint::Read(h) => {
                let ctx = ReadCtx::new(admitted.principal, deadline, shared.clone());
                until_deadline(shared, deadline, || h.call(ctx, request())).await?
            }
            Endpoint::Write(h) => {
                let ctx = WriteCtx::new(admitted.principal, deadline, shared.clone());
                h.call(ctx, request()).await
            }
            Endpoint::Upgrade(h) => {
                let (Some(principal), Some((ticket_sha, entry))) =
                    (admitted.principal, admitted.ticket)
                else {
                    return Err(ApiError::internal());
                };
                upgrade::admit(
                    shared.clone(),
                    principal,
                    ticket_sha,
                    entry.purpose,
                    deadline,
                    request(),
                    h.clone(),
                )
                .await
            }
        }
    }

    async fn credential(&self, req: &Parts, deadline: Instant) -> Result<Admitted, ApiError> {
        let shared = &self.shared;
        match self.spec.credential {
            Credential::None | Credential::PairCode => Ok(Admitted {
                principal: None,
                ticket: None,
            }),
            Credential::Bearer(_) => {
                let token = parse_bearer(&req.headers).ok_or_else(ApiError::unauthorized)?;
                Ok(Admitted {
                    principal: Some(verified(shared, deadline, sha256(token)).await?),
                    ticket: None,
                })
            }
            Credential::Ticket(kind) => {
                let ticket = ticket_param(req.uri.query())?;
                let ticket_sha = sha256(ticket);
                // Peek: nothing is removed until every fallible check has passed.
                let entry = shared
                    .tickets
                    .peek(&ticket_sha, shared.cfg.clock.now())
                    .ok_or_else(ApiError::unauthorized)?;
                let purpose_ok = match (kind, entry.purpose) {
                    (TicketPurposeKind::Events, TicketPurpose::Events) => true,
                    (TicketPurposeKind::Attach, TicketPurpose::Attach(t)) => {
                        path_param(self.spec.path, req.uri.path(), "thread")
                            .is_some_and(|p| p == t.to_string())
                    }
                    _ => false,
                };
                if !purpose_ok {
                    return Err(ApiError::unauthorized());
                }
                // The credential read: the ticket's token is still valid; the ticket is kept
                // whatever this read answers.
                Ok(Admitted {
                    principal: Some(verified(shared, deadline, entry.token_sha256).await?),
                    ticket: Some((ticket_sha, entry)),
                })
            }
        }
    }

    fn kind_and_action(&self, p: &Principal, path: &str) -> Result<(), ApiError> {
        let spec = self.spec;
        if let Credential::Bearer(kinds) = spec.credential
            && !kinds.contains(&p.kind)
        {
            return Err(ApiError::forbidden());
        }
        let action = match spec.action {
            ActionRule::None => return Ok(()),
            ActionRule::Fixed(a) => a,
            ActionRule::ByCommandName => {
                let name = path_param(spec.path, path, "name").unwrap_or_default();
                self.shared
                    .commands
                    .spec(name)
                    .ok_or_else(ApiError::not_found)?
                    .action
            }
        };
        if grant(p.kind, action).is_some() {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }
}

/// The token's check with the request's cutoff; its id is recorded on the request span.
async fn verified(
    shared: &Arc<Shared>,
    deadline: Instant,
    sha: [u8; 32],
) -> Result<Principal, ApiError> {
    let auth = verify_until(shared, deadline, sha).await?;
    Span::current().record("token_id", tracing::field::display(auth.token_id));
    Ok(Principal {
        token_id: auth.token_id,
        token_sha256: sha,
        kind: auth.kind,
        thread: auth.thread,
    })
}

/// The `Origin` rule: at most one header, compared with `http://` + the received `Host`.
fn check_origin(rule: OriginRule, headers: &HeaderMap) -> Result<(), ApiError> {
    if rule == OriginRule::NotChecked {
        return Ok(());
    }
    let origins: Vec<&HeaderValue> = headers.get_all(header::ORIGIN).iter().collect();
    let matching = || {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        match (origins.first().and_then(|o| o.to_str().ok()), host) {
            (Some(o), Some(h)) => o.strip_prefix("http://") == Some(h),
            _ => false,
        }
    };
    let ok = match (rule, origins.len()) {
        (OriginRule::AbsentOrMatching | OriginRule::MustBeAbsent, 0) => true,
        (OriginRule::AbsentOrMatching | OriginRule::PresentMatching, 1) => matching(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ApiError::origin_rejected())
    }
}

/// Steps 6 and 7: any `Transfer-Encoding` is 400 (checked first: hyper has removed
/// `Content-Length` when both came); `POST` needs exactly one `Content-Length` (411) within
/// the body bound (413); `GET` has none other than a zero length, however spelled (400).
fn check_framing(method: &Method, headers: &HeaderMap, max: u64) -> Result<(), ApiError> {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(ApiError::bad_request());
    }
    let length = |v: &HeaderValue| v.to_str().ok().and_then(|v| v.parse::<u64>().ok());
    let lengths: Vec<&HeaderValue> = headers.get_all(header::CONTENT_LENGTH).iter().collect();
    if *method == Method::POST {
        let [len] = lengths.as_slice() else {
            return Err(ApiError::length_required());
        };
        let len = length(len).ok_or_else(ApiError::bad_request)?;
        if len > max {
            return Err(ApiError::payload_too_large());
        }
    } else if lengths.iter().any(|v| length(v) != Some(0)) {
        return Err(ApiError::bad_request());
    }
    Ok(())
}

/// The ticket of a query that is exactly one `ticket` parameter: a missing or repeated
/// `ticket` is 401, any other parameter (an empty one included) 400 `bad_query`.
fn ticket_param(query: Option<&str>) -> Result<&str, ApiError> {
    let mut ticket = None;
    let mut count = 0;
    // An absent or empty query has no parameter at all.
    let pairs = query.filter(|q| !q.is_empty()).map(|q| q.split('&'));
    for pair in pairs.into_iter().flatten() {
        match pair.split_once('=') {
            Some(("ticket", v)) => {
                count += 1;
                ticket = Some(v);
            }
            _ => return Err(ApiError::bad_query()),
        }
    }
    match ticket {
        Some(t) if count == 1 && has_secret_form(t, TICKET_PREFIX) => Ok(t),
        _ => Err(ApiError::unauthorized()),
    }
}

/// The path segment matching `{name}` in the route template.
pub(crate) fn path_param<'a>(template: &str, path: &'a str, name: &str) -> Option<&'a str> {
    let want = format!("{{{name}}}");
    let mut segments = path.split('/');
    for t in template.split('/') {
        let s = segments.next()?;
        if t == want {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_authority() {
        assert!(is_own_authority("127.0.0.1:8080", 8080));
        assert!(is_own_authority("LocalHost:8080", 8080));
        for bad in [
            "127.0.0.1:8081",
            "127.0.0.1",
            "evil:8080",
            "user@127.0.0.1:8080",
            "127.0.0.1:08080",
            "localhost.:8080",
            "[::1]:8080",
        ] {
            assert!(!is_own_authority(bad, 8080), "{bad}");
        }
    }

    #[test]
    fn ticket_query() {
        let t = format!("chdt_{}", "a".repeat(43));
        assert_eq!(ticket_param(Some(&format!("ticket={t}"))), Ok(t.as_str()));
        let unauthorized = Err(ApiError::unauthorized());
        assert_eq!(ticket_param(None), unauthorized);
        assert_eq!(
            ticket_param(Some(&format!("ticket={t}&ticket={t}"))),
            unauthorized
        );
        assert_eq!(ticket_param(Some("ticket=chdt_short")), unauthorized);
        assert_eq!(
            ticket_param(Some(&format!("ticket={t}&x=1"))),
            Err(ApiError::bad_query())
        );
        assert_eq!(ticket_param(Some("ticketx")), Err(ApiError::bad_query()));
        // Empty segments are parameters too.
        for q in [format!("ticket={t}&"), format!("&&ticket={t}"), "&".into()] {
            assert_eq!(ticket_param(Some(&q)), Err(ApiError::bad_query()), "{q}");
        }
    }

    #[test]
    fn path_params() {
        assert_eq!(
            path_param("/api/commands/{name}", "/api/commands/key", "name"),
            Some("key")
        );
        assert_eq!(
            path_param("/api/attach/{thread}", "/api/attach/x", "thread"),
            Some("x")
        );
        assert_eq!(path_param("/api/ws", "/api/ws", "thread"), None);
    }
}
