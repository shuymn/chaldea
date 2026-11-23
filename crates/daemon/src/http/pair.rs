//! Pairing (design §2.4 "Pair codes", §8 D12): `POST /api/pair-codes` mints a single-use
//! code bound to the caller's token; `POST /api/pair` spends it and issues a new `owner`
//! token in one write that first checks the issuer is still valid.

use axum::body::Body;
use axum::http::{Request, Response};
use chaldea_core::http::{PairCodeResponse, PairResponse};
use serde_json::{Map, Value};

use super::auth::{PAIR_CODE_PREFIX, TokenService, has_secret_form, sha256};
use super::ctx::{ReadCtx, WriteCtx};
use super::error::ApiError;
use super::sync_api::json;
use super::tickets::PairCodeEntry;

/// A JSON object body, else 400 `bad_request`; an empty body is an empty object.
pub(crate) fn object_body(bytes: &[u8]) -> Result<Map<String, Value>, ApiError> {
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(ApiError::bad_request()),
    }
}

/// Whole seconds of a lifetime, for `expires_in`.
pub(crate) fn secs(d: std::time::Duration) -> u32 {
    u32::try_from(d.as_secs()).unwrap_or(u32::MAX)
}

/// `POST /api/pair-codes` (owner Bearer, `Origin` absent): the body is empty or `{}`.
pub(crate) async fn pair_codes(
    ctx: ReadCtx,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let shared = ctx.shared().clone();
    let body = ctx
        .body_within(req.into_body(), shared.cfg.max_body_bytes)
        .await?;
    if !object_body(&body)?.is_empty() {
        return Err(ApiError::bad_request());
    }
    let issuer = ctx.principal().ok_or_else(ApiError::internal)?;
    let now = shared.cfg.clock.now();
    let ttl = shared.cfg.pair_code_ttl;
    let code = shared
        .pair_codes
        .mint(
            PairCodeEntry {
                issuer_sha256: issuer.token_sha256,
                expires_at: now + ttl,
            },
            now,
        )
        .map_err(|_| ApiError::busy())?;
    json(&PairCodeResponse {
        code: code.expose().to_owned(),
        expires_in: secs(ttl),
    })
}

/// `POST /api/pair` (`Origin` present and matching) with `PairRequest { code }`: a body
/// that is not a JSON object (an empty one included), or has another member, is 400; a
/// missing, malformed, unknown, spent, or expired code is 401.
pub(crate) async fn pair(ctx: WriteCtx, req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let shared = ctx.shared().clone();
    let body = ctx
        .body_within(req.into_body(), shared.cfg.max_body_bytes)
        .await?;
    if body.is_empty() {
        return Err(ApiError::bad_request());
    }
    let mut members = object_body(&body)?;
    let code = members.remove("code");
    if !members.is_empty() {
        return Err(ApiError::bad_request());
    }
    let code = match &code {
        Some(Value::String(c)) if has_secret_form(c, PAIR_CODE_PREFIX) => c,
        _ => return Err(ApiError::unauthorized()),
    };
    // 1. The conditional removal: the code is spent from here on.
    let entry = shared
        .pair_codes
        .take(&sha256(code), &*shared.cfg.clock)
        .ok_or_else(ApiError::unauthorized)?;
    // 2. One write: the issuer verified, then the new owner token inserted.
    let token = TokenService::new(shared)
        .issue_owner(Some(entry.issuer_sha256), ctx.deadline())
        .await?;
    json(&PairResponse {
        token: token.expose().to_owned(),
    })
}
