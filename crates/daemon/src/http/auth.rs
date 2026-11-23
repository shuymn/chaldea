//! Secrets, tokens, and the one token check (design §2.4, §2.5).
//!
//! A secret is 32 bytes from the OS CSPRNG, base64url without padding, behind a fixed
//! prefix. Only its SHA-256 is stored and looked up; the final equality uses `subtle`.
//! After a request is verified only the token's id and hash travel.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chaldea_core::auth::{
    Action, Grant, ThreadGraph, TokenKind, grant, may_act_on_thread, surface_in_scope,
};
use chaldea_core::ids::{LaunchId, StreamId, SurfaceId, ThreadId, TokenId, WorkspaceId};
use chaldea_core::payload::SurfaceScope;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::Shared;
use super::ctx::await_write;
use super::error::{ApiError, refused};
use crate::store::{JobError, NewToken, ReadTx, StoreError, TokenSelector, WriteTx};

pub(crate) const TOKEN_PREFIX: &str = "chd_";
pub(crate) const TICKET_PREFIX: &str = "chdt_";
pub(crate) const PAIR_CODE_PREFIX: &str = "chdp_";
/// base64url without padding of 32 bytes.
const SECRET_CHARS: usize = 43;

/// A token, ticket, or pair code in plain text. No `Display`; `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Secret {
    /// A new secret with `prefix` from the OS CSPRNG.
    ///
    /// # Panics
    ///
    /// When the OS random source fails (no secret can be made safely then).
    pub(crate) fn generate(prefix: &str) -> Secret {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the OS random source failed");
        Secret(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// The plain text, for the one reply that hands it out.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The SHA-256 of the secret's bytes, as stored.
    #[must_use]
    pub fn sha256(&self) -> [u8; 32] {
        sha256(&self.0)
    }
}

pub(crate) fn sha256(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Whether `s` is `prefix` followed by exactly 43 base64url characters.
pub(crate) fn has_secret_form(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|rest| {
        rest.len() == SECRET_CHARS
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// The token of exactly one `Authorization: Bearer <token>` header: the scheme `Bearer`
/// (ASCII case-insensitive), one space, and a `chd_` token, nothing else.
pub(crate) fn parse_bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let (scheme, token) = value.split_at_checked(7)?;
    let scheme = scheme.strip_suffix(' ')?;
    (scheme.eq_ignore_ascii_case("Bearer") && has_secret_form(token, TOKEN_PREFIX)).then_some(token)
}

/// A freshly minted token: its id and secret, not yet stored.
pub struct MintedToken {
    pub token_id: TokenId,
    pub secret: Secret,
    pub sha256: [u8; 32],
}

impl fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedToken")
            .field("token_id", &self.token_id)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}

/// Who a verified request is: what travels after verification (no plaintext).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub token_id: TokenId,
    pub token_sha256: [u8; 32],
    pub kind: TokenKind,
    pub thread: Option<ThreadId>,
}

/// The result of [`verify_hash_in`] inside one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorized {
    pub token_id: TokenId,
    pub kind: TokenKind,
    pub thread: Option<ThreadId>,
    pub launch: Option<LaunchId>,
}

/// Why a token check refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// Missing, revoked, malformed, an agent token whose launch is not current or has
    /// exited, or a hook token without its thread and launch or whose launch is not of its
    /// thread: 401.
    Invalid,
    /// The lookup failed: mapped through the store mapping, never 401.
    Store(StoreError),
}

impl From<StoreError> for AuthFailure {
    fn from(e: StoreError) -> Self {
        AuthFailure::Store(e)
    }
}

/// The one token check: by hash, in the caller's transaction (design §2.4).
///
/// # Errors
///
/// [`AuthFailure`].
pub fn verify_hash_in(tx: &ReadTx<'_>, sha256: &[u8; 32]) -> Result<Authorized, AuthFailure> {
    let Some(row) = tx.token_by_hash(sha256)? else {
        return Err(AuthFailure::Invalid);
    };
    if !bool::from(row.sha256.ct_eq(sha256)) || row.revoked_at.is_some() {
        return Err(AuthFailure::Invalid);
    }
    if row.kind == TokenKind::Agent {
        // One launch: the token's launch is its thread's current one and has not exited.
        let (Some(t), Some(l)) = (row.thread_id, row.launch_id) else {
            return Err(AuthFailure::Invalid);
        };
        let current = tx.thread(t)?.and_then(|r| r.current_launch_id);
        if current != Some(l) {
            return Err(AuthFailure::Invalid);
        }
        match tx.launch(l)? {
            Some(launch) if launch.exit_reason.is_none() => {}
            _ => return Err(AuthFailure::Invalid),
        }
    }
    if row.kind == TokenKind::Hook {
        // One launch of one thread; it need not be current, live, or bound: late hooks are
        // kept, and revocation ends the token (hook-ingest §2.8).
        let (Some(t), Some(l)) = (row.thread_id, row.launch_id) else {
            return Err(AuthFailure::Invalid);
        };
        match tx.launch(l)? {
            Some(launch) if launch.thread_id == t => {}
            _ => return Err(AuthFailure::Invalid),
        }
    }
    Ok(Authorized {
        token_id: row.token_id,
        kind: row.kind,
        thread: row.thread_id,
        launch: row.launch_id,
    })
}

/// Parses a `chd_` token, hashes it, and runs [`verify_hash_in`].
///
/// # Errors
///
/// [`AuthFailure`]; a malformed secret is `Invalid` without a lookup.
pub fn verify_secret_in(tx: &ReadTx<'_>, secret: &str) -> Result<Authorized, AuthFailure> {
    if !has_secret_form(secret, TOKEN_PREFIX) {
        return Err(AuthFailure::Invalid);
    }
    verify_hash_in(tx, &sha256(secret))
}

/// [`verify_hash_in`] inside a read: an invalid token is the reply 401, a failed lookup
/// the read's own error (mapped through §2.9, never 401).
pub(crate) fn verify_in_read(
    tx: &ReadTx<'_>,
    sha256: &[u8; 32],
) -> Result<Result<Authorized, ApiError>, StoreError> {
    match verify_hash_in(tx, sha256) {
        Ok(a) => Ok(Ok(a)),
        Err(AuthFailure::Invalid) => Ok(Err(ApiError::unauthorized())),
        Err(AuthFailure::Store(e)) => Err(e),
    }
}

/// [`verify_hash_in`] inside a write job, in its transaction, when there is a hash: an
/// invalid token refuses the job with 401, a failed lookup is the job's store error.
pub(crate) fn verify_opt_in_job(
    tx: &WriteTx<'_>,
    sha256: Option<[u8; 32]>,
) -> Result<Option<Authorized>, JobError> {
    let Some(sha256) = sha256 else {
        return Ok(None);
    };
    verify_hash_in(&tx.read(), &sha256)
        .map(Some)
        .map_err(|e| match e {
            AuthFailure::Invalid => refused(ApiError::unauthorized()),
            AuthFailure::Store(e) => e.into(),
        })
}

/// Token issuance, verification, and revocation.
#[derive(Clone)]
pub struct TokenService {
    shared: Arc<Shared>,
}

impl fmt::Debug for TokenService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenService").finish_non_exhaustive()
    }
}

impl TokenService {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        TokenService { shared }
    }

    /// A random id (v4) and secret; touches nothing.
    #[must_use]
    pub fn mint(&self) -> MintedToken {
        let secret = Secret::generate(TOKEN_PREFIX);
        MintedToken {
            token_id: TokenId(Uuid::new_v4()),
            sha256: secret.sha256(),
            secret,
        }
    }

    /// The row to insert for `m`.
    #[must_use]
    pub fn new_token(
        m: &MintedToken,
        kind: TokenKind,
        thread: Option<ThreadId>,
        launch: Option<LaunchId>,
    ) -> NewToken {
        NewToken {
            token_id: m.token_id,
            kind,
            sha256: m.sha256,
            thread_id: thread,
            launch_id: launch,
        }
    }

    /// Issues a new `owner` token in one write by `deadline` (on the server's clock). With
    /// `issuer`, the issuing token is verified in the same transaction first (a revoked
    /// issuer: 401, nothing inserted).
    ///
    /// # Errors
    ///
    /// The mapped write outcome; 503 `outcome_unknown` when the writer has not replied by
    /// `deadline` plus the reply grace.
    pub async fn issue_owner(
        &self,
        issuer: Option<[u8; 32]>,
        deadline: Instant,
    ) -> Result<Secret, ApiError> {
        let minted = self.mint();
        let row = Self::new_token(&minted, TokenKind::Owner, None, None);
        await_write(
            &self.shared,
            deadline,
            Box::new(move |tx| {
                verify_opt_in_job(tx, issuer)?;
                tx.token_insert(row)?;
                Ok(())
            }),
            None,
        )
        .await?;
        Ok(minted.secret)
    }

    /// Revokes the selected tokens in one write; the revocation nudge fires from the task
    /// observing the commit, also when the caller has gone.
    ///
    /// # Errors
    ///
    /// As [`TokenService::issue_owner`].
    pub async fn revoke(&self, which: TokenSelector, deadline: Instant) -> Result<u64, ApiError> {
        let shared = self.shared.clone();
        await_write(
            &self.shared,
            deadline,
            Box::new(move |tx| Ok(tx.token_revoke(which)?)),
            Some(Box::new(move || shared.nudge())),
        )
        .await
    }
}

/// The refusal for an object `auth` may not act on: 404 when its thread does not exist or
/// `auth` cannot read it (identical to a missing one), else 403 (design §2.5 "Hiding").
#[must_use]
pub fn deny_for(g: &ThreadGraph, auth: &Authorized, target: ThreadId) -> ApiError {
    let readable = g.contains(target)
        && grant(auth.kind, Action::ReadStream)
            .is_some_and(|gr| may_act_on_thread(g, auth.thread, gr, target));
    if readable {
        ApiError::forbidden()
    } else {
        ApiError::not_found()
    }
}

/// Object checks on stored objects (design §2.5 "Object checks").
#[derive(Debug, Clone, Copy)]
pub struct ObjectScope;

impl ObjectScope {
    /// Whether the stored surface `surface_id` of `stream` exists and is in the caller's
    /// `POST /events` scope; `configured` is an extension token's scope (PR 20), `None`
    /// until it exists.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn stored_surface(
        tx: &ReadTx<'_>,
        stream: StreamId,
        surface_id: &SurfaceId,
        auth: &Authorized,
        configured: Option<&SurfaceScope>,
    ) -> Result<bool, StoreError> {
        let Some(row) = tx.surface(stream, surface_id)? else {
            return Ok(false);
        };
        let Some(g): Option<Grant> = grant(auth.kind, Action::PostEvents) else {
            return Ok(false);
        };
        let scope = &row.update.scope;
        let thread_ws: Option<(ThreadId, WorkspaceId)> = match scope {
            SurfaceScope::Thread { thread_id } => {
                tx.thread(*thread_id)?.map(|r| (*thread_id, r.workspace_id))
            }
            _ => None,
        };
        Ok(surface_in_scope(g, configured, scope, |t| {
            thread_ws
                .as_ref()
                .filter(|(id, _)| *id == t)
                .map(|(_, w)| w.clone())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let tok = format!("chd_{}", "a".repeat(43));
        let ok = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, v.parse().unwrap());
            parse_bearer(&h).map(str::to_owned)
        };
        assert_eq!(ok(&format!("Bearer {tok}")).as_deref(), Some(tok.as_str()));
        assert_eq!(ok(&format!("bEARER {tok}")).as_deref(), Some(tok.as_str()));
        for bad in [
            format!("Bearer  {tok}"),
            format!("Basic {tok}"),
            format!("Bearer {tok}x"),
            format!("Bearer {tok} "),
            format!("Bearer\t{tok}"),
            "Bearer chd_short".to_owned(),
            format!("Bearer chdt_{}", "a".repeat(43)),
            format!("Bearer chd_{}!", "a".repeat(42)),
        ] {
            assert_eq!(ok(&bad), None, "{bad:?}");
        }
        let mut two = HeaderMap::new();
        two.append(
            header::AUTHORIZATION,
            format!("Bearer {tok}").parse().unwrap(),
        );
        two.append(
            header::AUTHORIZATION,
            format!("Bearer {tok}").parse().unwrap(),
        );
        assert_eq!(parse_bearer(&two), None);
        assert_eq!(parse_bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn secrets_have_their_form_and_are_redacted() {
        let s = Secret::generate(TOKEN_PREFIX);
        assert!(has_secret_form(s.expose(), TOKEN_PREFIX));
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_ne!(s, Secret::generate(TOKEN_PREFIX));
        let t = Secret::generate(TICKET_PREFIX);
        assert!(has_secret_form(t.expose(), TICKET_PREFIX));
        assert!(!has_secret_form(t.expose(), TOKEN_PREFIX));
    }
}
