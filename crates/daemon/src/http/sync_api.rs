//! The sync read routes (design §2.7): `GET /api/events`, `GET /api/threads`,
//! `GET /api/threads/{thread_id}`, `GET /api/worktrees`. Each answer comes from one read
//! transaction: the token check, the readability check, the head, and the page or snapshot.

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, header};
use chaldea_core::MAX_SAFE_INTEGER;
use chaldea_core::auth::Action;
use chaldea_core::http::{ThreadsResponse, WorktreesResponse};
use chaldea_core::ids::{Epoch, StreamId, ThreadId};
use chaldea_core::sync::{EventsQuery, EventsResponse, ReplayPlan, StreamSnapshot, decide_replay};
use serde::Serialize;

use super::ctx::ReadCtx;
use super::error::{ApiError, read_error};
use super::guard::path_param;
use super::scope::ReadScope;
use crate::store::{ReadError, StoreError};

/// A store error inside a read closure, through the read mapping (design §2.9).
pub(crate) fn store(e: StoreError) -> ApiError {
    read_error(&ReadError::Store(e))
}

/// A 200 with `v` as JSON.
pub(crate) fn json<T: Serialize>(v: &T) -> Result<Response<Body>, ApiError> {
    let body = serde_json::to_vec(v).map_err(|_| ApiError::internal())?;
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(resp)
}

/// Percent-decodes one query component (`+` is a space, as in form encoding); `None` for an
/// escape that is not `%` and two ASCII hex digits, or text that is not UTF-8.
fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                let hex = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The query's parameters, decoded: each must be `name=value` with a name in `allowed`,
/// at most once (an empty segment is a parameter too). Else 400 `bad_query`.
pub(crate) fn params(
    query: Option<&str>,
    allowed: &[&str],
) -> Result<Vec<(String, String)>, ApiError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        return Ok(out);
    };
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').ok_or_else(ApiError::bad_query)?;
        let (k, v) = (
            decode(k).ok_or_else(ApiError::bad_query)?,
            decode(v).ok_or_else(ApiError::bad_query)?,
        );
        if !allowed.contains(&k.as_str()) || out.iter().any(|(seen, _)| *seen == k) {
            return Err(ApiError::bad_query());
        }
        out.push((k, v));
    }
    Ok(out)
}

/// A decimal `u64` up to `MAX_SAFE_INTEGER`, digits only.
fn safe_u64(v: &str) -> Result<u64, ApiError> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_query());
    }
    v.parse::<u64>()
        .ok()
        .filter(|n| *n <= MAX_SAFE_INTEGER)
        .ok_or_else(ApiError::bad_query)
}

/// A `StreamId`, else 400 `bad_query`.
pub(crate) fn stream_id(v: &str) -> Result<StreamId, ApiError> {
    v.parse().map_err(|_| ApiError::bad_query())
}

/// The query of `GET /api/events` (design §2.7): `stream_id` required; `after_seq`,
/// `before_seq`, `limit`, `epoch` optional; anything else, a repeat, or a malformed value
/// is 400 `bad_query`.
pub(crate) fn events_query(query: Option<&str>) -> Result<EventsQuery, ApiError> {
    let mut q = EventsQuery {
        stream_id: StreamId::Host,
        after_seq: None,
        before_seq: None,
        limit: None,
        epoch: None,
    };
    let mut stream = None;
    for (k, v) in params(
        query,
        &["stream_id", "after_seq", "before_seq", "limit", "epoch"],
    )? {
        match k.as_str() {
            "stream_id" => stream = Some(stream_id(&v)?),
            "after_seq" => q.after_seq = Some(safe_u64(&v)?),
            "before_seq" => q.before_seq = Some(safe_u64(&v)?),
            "limit" => {
                let n = safe_u64(&v)?;
                q.limit = Some(u32::try_from(n).map_err(|_| ApiError::bad_query())?);
            }
            _ => q.epoch = Some(Epoch(v)),
        }
    }
    q.stream_id = stream.ok_or_else(ApiError::bad_query)?;
    Ok(q)
}

/// `GET /api/events` (design §2.7).
pub(crate) async fn events(ctx: ReadCtx, req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let q = events_query(req.uri().query())?;
    let epoch = ctx.epoch().clone();
    let reply = ctx
        .read(move |tx, auth| {
            let stream = q.stream_id;
            // 1. The stream exists and is readable: one 404 for both.
            let scope = ReadScope::of(tx, auth, Action::ReadStream).map_err(store)?;
            if !scope.readable_stream(tx, stream).map_err(store)? {
                return Err(ApiError::not_found());
            }
            // 2. The head and the decision, from the same snapshot.
            let head = tx.head(stream, q.after_seq.unwrap_or(0)).map_err(store)?;
            let plan = decide_replay(&q, head, &epoch).map_err(|_| ApiError::bad_query())?;
            // 3. The page or the snapshot.
            let page = |p: crate::store::Page| EventsResponse::Replay {
                stream_id: stream,
                epoch: epoch.clone(),
                events: p.events,
                head_seq: head.head_seq,
                has_more: p.has_more,
                has_more_before: p.has_more_before,
            };
            Ok(match plan {
                ReplayPlan::Forward { after_seq, limit } => {
                    page(tx.events_after(stream, after_seq, limit).map_err(store)?)
                }
                ReplayPlan::Backward { before_seq, limit } => {
                    page(tx.events_before(stream, before_seq, limit).map_err(store)?)
                }
                ReplayPlan::Snapshot { reason } => {
                    let (snapshot, as_of_seq) = match stream {
                        StreamId::Host => {
                            let (s, head) = tx.host_snapshot().map_err(store)?;
                            (StreamSnapshot::Host(s), head)
                        }
                        StreamId::Thread(t) => {
                            let (mut s, head) = tx
                                .thread_snapshot(t)
                                .map_err(store)?
                                .ok_or_else(ApiError::internal)?;
                            // The launch's worktree under `/api/worktrees`'s rule.
                            let worktrees =
                                ReadScope::of(tx, auth, Action::ReadWorktrees).map_err(store)?;
                            if s.worktree.as_ref().is_some_and(|w| !worktrees.worktree(w)) {
                                s.worktree = None;
                            }
                            (StreamSnapshot::Thread(Box::new(s)), head)
                        }
                    };
                    EventsResponse::Snapshot {
                        stream_id: stream,
                        epoch: epoch.clone(),
                        snapshot,
                        as_of_seq,
                        reason,
                    }
                }
            })
        })
        .await?;
    json(&reply)
}

/// A route that takes no query: any parameter is 400 `bad_query`.
fn no_query(req: &Request<Body>) -> Result<(), ApiError> {
    params(req.uri().query(), &[]).map(|_| ())
}

/// `GET /api/threads`: the threads the token can read.
pub(crate) async fn threads(ctx: ReadCtx, req: Request<Body>) -> Result<Response<Body>, ApiError> {
    no_query(&req)?;
    let epoch = ctx.epoch().clone();
    let threads = ctx
        .read(|tx, auth| {
            let scope = ReadScope::of(tx, auth, Action::ReadThreadList).map_err(store)?;
            Ok(tx
                .thread_infos()
                .map_err(store)?
                .into_iter()
                .filter(|t| scope.thread(t.thread_id))
                .collect())
        })
        .await?;
    json(&ThreadsResponse { epoch, threads })
}

/// `GET /api/threads/{thread_id}`: the thread's [`ThreadSummary`](chaldea_core::sync::ThreadSummary),
/// authorized as `GET /api/events` authorizes its stream. A thread the token cannot read, a
/// missing thread, and an unparseable id are one 404.
pub(crate) async fn thread_summary(
    ctx: ReadCtx,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    no_query(&req)?;
    // The canonical form only, as a thread stream id spells it.
    let t: ThreadId = match path_param(THREAD_PATH, req.uri().path(), "thread_id")
        .and_then(|p| format!("thread:{p}").parse().ok())
    {
        Some(StreamId::Thread(t)) => t,
        _ => return Err(ApiError::not_found()),
    };
    let summary = ctx
        .read(move |tx, auth| {
            let scope = ReadScope::of(tx, auth, Action::ReadStream).map_err(store)?;
            if !scope
                .readable_stream(tx, StreamId::Thread(t))
                .map_err(store)?
            {
                return Err(ApiError::not_found());
            }
            tx.thread_summary(t)
                .map_err(store)?
                .ok_or_else(ApiError::not_found)
        })
        .await?;
    json(&summary)
}

/// The path of [`thread_summary`].
pub(crate) const THREAD_PATH: &str = "/api/threads/{thread_id}";

/// `GET /api/worktrees`: all for `owner` and `rescue`, human-owned included; for `agent`,
/// those whose current owner is in self+.
pub(crate) async fn worktrees(
    ctx: ReadCtx,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    no_query(&req)?;
    let epoch = ctx.epoch().clone();
    let worktrees = ctx
        .read(|tx, auth| {
            let scope = ReadScope::of(tx, auth, Action::ReadWorktrees).map_err(store)?;
            Ok(tx
                .worktrees()
                .map_err(store)?
                .into_iter()
                .map(|w| w.record)
                .filter(|w| scope.worktree(w))
                .collect())
        })
        .await?;
    json(&WorktreesResponse { epoch, worktrees })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_rules() {
        let t = "thread:00000000-0000-0000-0000-000000000001";
        let q = events_query(Some(&format!(
            "stream_id={}&after_seq=3&limit=7&epoch=e%201",
            t.replace(':', "%3A")
        )))
        .unwrap();
        assert_eq!(q.stream_id.to_string(), t);
        assert_eq!((q.after_seq, q.limit), (Some(3), Some(7)));
        assert_eq!(q.epoch, Some(Epoch("e 1".into())));
        let bad = Err(ApiError::bad_query());
        for s in [
            "",
            "after_seq=1",
            "stream_id=host&stream_id=host",
            "stream_id=host&x=1",
            "stream_id=host&",
            "stream_id=nope",
            "stream_id=host&after_seq=-1",
            "stream_id=host&after_seq=+1",
            "stream_id=host&after_seq=",
            "stream_id=host&after_seq=9007199254740992",
            "stream_id=host&limit=4294967296",
            "stream_id=host&limit",
            "stream_id=host&epoch=%zz",
            "stream_id=host&epoch=%ff",
            "stream_id=host&epoch=%+1",
            "stream_id=host&epoch=%-1",
            "stream_id=host&epoch=%1",
        ] {
            assert_eq!(events_query(Some(s)), bad, "{s}");
        }
        let max = events_query(Some("stream_id=host&before_seq=9007199254740991")).unwrap();
        assert_eq!(max.before_seq, Some(MAX_SAFE_INTEGER));
        assert_eq!(events_query(None), bad);
    }
}
