//! `X-Chaldea-*` header parsing (`docs/design/hook-ingest.md` §2.2), after the guard and
//! before the body.

use axum::http::HeaderMap;
use chaldea_core::ids::ThreadId;
use uuid::Uuid;

use crate::http::ApiError;

pub(crate) const EMIT_ID: &str = "x-chaldea-emit-id";
pub(crate) const ATTEMPT: &str = "x-chaldea-attempt";
pub(crate) const EMITTER_PID: &str = "x-chaldea-emitter-pid";
pub(crate) const THREAD_ID: &str = "x-chaldea-thread-id";
pub(crate) const AGENT: &str = "x-chaldea-agent";
pub(crate) const EMITTER_VERSION: &str = "x-chaldea-emitter-version";
pub(crate) const BODY_TRUNCATED: &str = "x-chaldea-body-truncated";

/// Recorded for an absent or empty agent or version.
pub(crate) const UNKNOWN: &str = "unknown";

/// The request's `X-Chaldea-*` values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookHeaders {
    pub(crate) emit_id: Uuid,
    /// `1..=3`; `None` when absent or outside it.
    pub(crate) attempt: Option<u8>,
    /// A decimal `u32` in `1..`; `None` otherwise (the chain is then empty).
    pub(crate) emitter_pid: Option<u32>,
    /// A hyphenated UUID; `None` otherwise (row 4 treats both alike).
    pub(crate) thread: Option<ThreadId>,
    /// As sent, or `unknown`; recorded only.
    pub(crate) agent: String,
    /// As sent, or `unknown`; recorded only.
    pub(crate) emitter_version: String,
    /// `X-Chaldea-Body-Truncated: 1`.
    pub(crate) truncated: bool,
}

/// The single value of `name`: `Ok(None)` when absent, 400 when repeated.
fn single<'h>(
    h: &'h HeaderMap,
    name: &str,
) -> Result<Option<&'h axum::http::HeaderValue>, ApiError> {
    let mut all = h.get_all(name).iter();
    let first = all.next();
    if all.next().is_some() {
        return Err(ApiError::bad_request());
    }
    Ok(first)
}

fn text<'h>(h: &'h HeaderMap, name: &str) -> Result<Option<&'h str>, ApiError> {
    Ok(single(h, name)?.and_then(|v| v.to_str().ok()))
}

/// A hyphenated UUID (8-4-4-4-12 hex digits) only.
pub(crate) fn hyphenated_uuid(s: &str) -> Option<Uuid> {
    let shape = s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    shape.then(|| Uuid::parse_str(s).ok()).flatten()
}

fn decimal(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses the listed headers; unlisted `X-Chaldea-*` headers are ignored.
///
/// # Errors
///
/// 400 `bad_request` when `X-Chaldea-Emit-Id` is missing, repeated, or not a hyphenated
/// UUID, or another listed header is repeated.
pub(crate) fn parse(h: &HeaderMap) -> Result<HookHeaders, ApiError> {
    let emit_id = text(h, EMIT_ID)?
        .and_then(hyphenated_uuid)
        .ok_or_else(ApiError::bad_request)?;
    let attempt = text(h, ATTEMPT)?
        .and_then(decimal)
        .filter(|a| (1..=3).contains(a))
        .and_then(|a| u8::try_from(a).ok());
    let emitter_pid = text(h, EMITTER_PID)?.and_then(decimal).filter(|p| *p >= 1);
    let thread = text(h, THREAD_ID)?.and_then(hyphenated_uuid).map(ThreadId);
    let recorded = |v: Option<&str>| v.filter(|s| !s.is_empty()).unwrap_or(UNKNOWN).to_owned();
    let agent = recorded(text(h, AGENT)?);
    let emitter_version = recorded(text(h, EMITTER_VERSION)?);
    let truncated = text(h, BODY_TRUNCATED)? == Some("1");
    Ok(HookHeaders {
        emit_id,
        attempt,
        emitter_pid,
        thread,
        agent,
        emitter_version,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0190c6f2-0000-7000-8000-000000000001";

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    /// The header table: required and repeated headers, forms outside their range, and
    /// unknown headers.
    #[test]
    fn header_table() {
        let full = parse(&map(&[
            ("X-Chaldea-Emit-Id", ID),
            ("X-Chaldea-Attempt", "2"),
            ("X-Chaldea-Emitter-Pid", "4242"),
            ("X-Chaldea-Thread-Id", ID),
            ("X-Chaldea-Agent", "codex"),
            ("X-Chaldea-Emitter-Version", "0.1.0"),
            ("X-Chaldea-Body-Truncated", "1"),
            ("X-Chaldea-Future", "a"),
            ("X-Chaldea-Future", "b"),
        ]))
        .unwrap();
        assert_eq!(
            full,
            HookHeaders {
                emit_id: Uuid::parse_str(ID).unwrap(),
                attempt: Some(2),
                emitter_pid: Some(4242),
                thread: Some(ThreadId(Uuid::parse_str(ID).unwrap())),
                agent: "codex".into(),
                emitter_version: "0.1.0".into(),
                truncated: true,
            }
        );
        let minimal = parse(&map(&[("x-chaldea-emit-id", &ID.to_uppercase())])).unwrap();
        assert_eq!(minimal.attempt, None);
        assert_eq!(minimal.emitter_pid, None);
        assert_eq!(minimal.thread, None);
        assert_eq!(minimal.agent, UNKNOWN);
        assert_eq!(minimal.emitter_version, UNKNOWN);
        assert!(!minimal.truncated);

        let bad = Err(ApiError::bad_request());
        assert_eq!(parse(&map(&[])), bad);
        for id in [
            "not-a-uuid",
            "0190c6f2000070008000000000000001",
            "{0190c6f2-0000-7000-8000-000000000001}",
            "urn:uuid:0190c6f2-0000-7000-8000-000000000001",
            "",
        ] {
            assert_eq!(parse(&map(&[("X-Chaldea-Emit-Id", id)])), bad, "{id}");
        }
        assert_eq!(
            parse(&map(&[
                ("X-Chaldea-Emit-Id", ID),
                ("X-Chaldea-Emit-Id", ID)
            ])),
            bad
        );
        for name in [
            ATTEMPT,
            EMITTER_PID,
            THREAD_ID,
            AGENT,
            EMITTER_VERSION,
            BODY_TRUNCATED,
        ] {
            assert_eq!(
                parse(&map(&[("X-Chaldea-Emit-Id", ID), (name, "1"), (name, "1")])),
                bad,
                "{name} repeated"
            );
        }
        // Values outside their form are absent.
        for (name, v) in [
            (ATTEMPT, "0"),
            (ATTEMPT, "4"),
            (ATTEMPT, "+1"),
            (EMITTER_PID, "0"),
            (EMITTER_PID, "+5"),
            (EMITTER_PID, "-1"),
            (EMITTER_PID, "4294967296"),
            (EMITTER_PID, "12a"),
            (THREAD_ID, "t"),
            (BODY_TRUNCATED, "yes"),
        ] {
            let h = parse(&map(&[("X-Chaldea-Emit-Id", ID), (name, v)])).unwrap();
            assert_eq!(
                (h.attempt, h.emitter_pid, h.thread, h.truncated),
                (None, None, None, false),
                "{name}: {v}"
            );
        }
        let empty = parse(&map(&[
            ("X-Chaldea-Emit-Id", ID),
            (AGENT, ""),
            (EMITTER_VERSION, ""),
        ]))
        .unwrap();
        assert_eq!(
            (empty.agent.as_str(), empty.emitter_version.as_str()),
            (UNKNOWN, UNKNOWN)
        );
    }
}
