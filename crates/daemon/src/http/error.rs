//! `ApiError` and the mapping of store outcomes to status codes (design §2.9).
//!
//! The store never chooses a status; this module is the one place that does. Every
//! rejection the daemon decides is an [`ErrorBody`] in `application/json`; hyper's own
//! parser responses are not (design §2.9 "Not application decisions").

use std::fmt;

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chaldea_core::http::{ErrorBody, ErrorCode};

use crate::store::{JobError, ReadError, StoreError, SubmitError, WriteError};

/// Whether `e` or an error in its source chain satisfies `is`: the walk for an error a
/// layer may have passed on boxed, tested by type.
pub(crate) fn in_source_chain(
    e: &(dyn std::error::Error + 'static),
    is: impl Fn(&(dyn std::error::Error + 'static)) -> bool,
) -> bool {
    std::iter::successors(Some(e), |err| err.source()).any(is)
}

/// Seconds a client waits before retrying a `busy` or `outcome_unknown` answer.
pub(crate) const RETRY_AFTER_SECS: u32 = 1;

/// A rejection: a status and an [`ErrorBody`]. It carries no secret and no body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    retry_after: Option<u32>,
    allow: Option<Method>,
}

impl ApiError {
    /// A rejection with `status` and `code` (command PRs add their own refusal codes).
    #[must_use]
    pub fn new(status: StatusCode, code: ErrorCode) -> ApiError {
        ApiError {
            status,
            code,
            retry_after: None,
            allow: None,
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, secs: u32) -> ApiError {
        self.retry_after = Some(secs);
        self
    }

    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    #[must_use]
    pub fn retry_after(&self) -> Option<u32> {
        self.retry_after
    }

    #[must_use]
    pub fn misdirected_request() -> ApiError {
        Self::new(
            StatusCode::MISDIRECTED_REQUEST,
            ErrorCode::MisdirectedRequest,
        )
    }

    #[must_use]
    pub fn origin_rejected() -> ApiError {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::OriginRejected)
    }

    #[must_use]
    pub fn unauthorized() -> ApiError {
        Self::new(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized)
    }

    #[must_use]
    pub fn forbidden() -> ApiError {
        Self::new(StatusCode::FORBIDDEN, ErrorCode::Forbidden)
    }

    #[must_use]
    pub fn not_found() -> ApiError {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound)
    }

    #[must_use]
    pub fn method_not_allowed(allow: Method) -> ApiError {
        ApiError {
            allow: Some(allow),
            ..Self::new(StatusCode::METHOD_NOT_ALLOWED, ErrorCode::MethodNotAllowed)
        }
    }

    #[must_use]
    pub fn request_timeout() -> ApiError {
        Self::new(StatusCode::REQUEST_TIMEOUT, ErrorCode::RequestTimeout)
    }

    #[must_use]
    pub fn length_required() -> ApiError {
        Self::new(StatusCode::LENGTH_REQUIRED, ErrorCode::LengthRequired)
    }

    #[must_use]
    pub fn payload_too_large() -> ApiError {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::PayloadTooLarge)
    }

    #[must_use]
    pub fn bad_request() -> ApiError {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadRequest)
    }

    #[must_use]
    pub fn bad_query() -> ApiError {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadQuery)
    }

    #[must_use]
    pub fn not_implemented() -> ApiError {
        Self::new(StatusCode::NOT_IMPLEMENTED, ErrorCode::NotImplemented)
    }

    #[must_use]
    pub fn busy() -> ApiError {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Busy)
            .with_retry_after(RETRY_AFTER_SECS)
    }

    #[must_use]
    pub fn disk_full() -> ApiError {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::DiskFull)
    }

    #[must_use]
    pub fn shutting_down() -> ApiError {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::ShuttingDown)
    }

    #[must_use]
    pub fn store_failed() -> ApiError {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::StoreFailed)
    }

    #[must_use]
    pub fn outcome_unknown() -> ApiError {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::OutcomeUnknown)
            .with_retry_after(RETRY_AFTER_SECS)
    }

    #[must_use]
    pub fn internal() -> ApiError {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
    }

    /// The body as sent.
    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code,
            retry_after: self.retry_after,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?}", self.status.as_u16(), self.code)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.body()).unwrap_or_default();
        let mut resp = (self.status, body).into_response();
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if self.status == StatusCode::UNAUTHORIZED {
            h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(m) = &self.allow
            && let Ok(v) = HeaderValue::from_str(m.as_str())
        {
            h.insert(header::ALLOW, v);
        }
        resp
    }
}

/// A store outcome of a read returned before its cutoff (design §2.3, §2.9).
pub(crate) fn read_error(e: &ReadError) -> ApiError {
    match e {
        ReadError::Busy | ReadError::Store(StoreError::Busy) => ApiError::busy(),
        ReadError::Closed => ApiError::shutting_down(),
        ReadError::Store(_) => ApiError::internal(),
    }
}

/// Why a write produced no value: refused at submission or answered with an error.
#[derive(Debug)]
pub(crate) enum WriteFailure {
    Submit(SubmitError),
    Write(WriteError),
}

/// The reply for a write without a receipt (design §2.8 table, §2.9 "Writes").
pub(crate) fn write_failure(f: WriteFailure) -> ApiError {
    match f {
        WriteFailure::Submit(SubmitError::QueueFull) => ApiError::busy(),
        WriteFailure::Submit(SubmitError::Closed) => ApiError::shutting_down(),
        WriteFailure::Submit(SubmitError::Failed) => ApiError::store_failed(),
        WriteFailure::Write(e) => match e {
            WriteError::Expired => ApiError::request_timeout(),
            WriteError::Busy | WriteError::RolledBack(StoreError::Busy) => ApiError::busy(),
            WriteError::RolledBack(StoreError::DiskFull) => ApiError::disk_full(),
            WriteError::Rejected(JobError::Refused(r)) => match r.downcast::<ApiError>() {
                Ok(api) => *api,
                Err(_) => ApiError::internal(),
            },
            WriteError::Rejected(JobError::Store(_))
            | WriteError::RolledBack(_)
            | WriteError::Panicked
            | WriteError::WouldDeadlock => ApiError::internal(),
            WriteError::Failed(_) => ApiError::store_failed(),
            WriteError::CommitUnknown(_) | WriteError::WriterGone => ApiError::outcome_unknown(),
        },
    }
}

/// A job's refusal carrying the reply it decided.
pub(crate) fn refused(e: ApiError) -> JobError {
    JobError::Refused(Box::new(e))
}
