//! `GET /api/blobs/{sha256}?stream_id=` (design §2.7): one read decides, in order, the
//! readable stream, the `blob_refs` row, the admission of a blob slot, and the bytes. The
//! slot's permit moves into the response body and is freed with it.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use chaldea_core::auth::Action;
use hyper::body::{Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::ctx::ReadCtx;
use super::error::ApiError;
use super::guard::path_param;
use super::scope::ReadScope;
use super::sync_api::{params, store, stream_id};

/// The frames a blob body yields, copied from the buffer it owns.
const FRAME_BYTES: usize = 64 * 1024;

/// The download type (mvp §2.5): the store's type only when it is one of these exactly.
fn download_type(stored: &str) -> &'static str {
    match stored {
        "text/plain; charset=utf-8" => "text/plain; charset=utf-8",
        "application/json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Whether `s` is 64 lowercase hex digits.
fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A blob's bytes and its slot: dropped together when hyper drops the body (sent,
/// connection ended, write stall, shutdown).
struct BlobBody {
    data: Vec<u8>,
    sent: usize,
    _permit: OwnedSemaphorePermit,
}

impl hyper::body::Body for BlobBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let start = self.sent;
        if start >= self.data.len() {
            return Poll::Ready(None);
        }
        let end = (start + FRAME_BYTES).min(self.data.len());
        let chunk = Bytes::copy_from_slice(&self.data[start..end]);
        self.sent = end;
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.sent >= self.data.len()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact((self.data.len() - self.sent) as u64)
    }
}

/// `GET /api/blobs/{sha256}?stream_id=`.
pub(crate) async fn download(ctx: ReadCtx, req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let sha = path_param("/api/blobs/{sha256}", req.uri().path(), "sha256")
        .filter(|s| is_sha256(s))
        .ok_or_else(ApiError::bad_query)?
        .to_owned();
    let stream = match params(req.uri().query(), &["stream_id"])?.as_slice() {
        [(_, v)] => stream_id(v)?,
        _ => return Err(ApiError::bad_query()),
    };
    let slots: Arc<Semaphore> = ctx.blob_slots();
    let (blob, permit) = ctx
        .read(move |tx, auth| {
            // 1. A readable stream.
            let scope = ReadScope::of(tx, auth, Action::ReadBlob).map_err(store)?;
            if !scope.readable_stream(tx, stream).map_err(store)? {
                return Err(ApiError::not_found());
            }
            // 2. A reference from that stream; the same 404 whether or not a slot is free.
            if !tx.blob_ref_exists(stream, &sha).map_err(store)? {
                return Err(ApiError::not_found());
            }
            // 3. Admission, non-blocking.
            let permit = slots.try_acquire_owned().map_err(|_| ApiError::busy())?;
            // 4. The bytes, in the same snapshot as the reference.
            let blob = tx
                .blob(stream, &sha)
                .map_err(store)?
                .ok_or_else(ApiError::not_found)?;
            Ok((blob, permit))
        })
        .await?;
    let len = blob.data.len();
    let ty = download_type(&blob.media_type);
    let mut resp = Response::new(Body::new(BlobBody {
        data: blob.data,
        sent: 0,
        _permit: permit,
    }));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ty));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment"),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_rule() {
        assert_eq!(
            download_type("text/plain; charset=utf-8"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(download_type("application/json"), "application/json");
        for t in [
            "text/html",
            "text/plain",
            "text/plain;charset=utf-8",
            "application/JSON",
            "image/svg+xml",
        ] {
            assert_eq!(download_type(t), "application/octet-stream", "{t}");
        }
    }

    #[test]
    fn sha_form() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"A".repeat(64)));
        assert!(!is_sha256(&"a".repeat(63)));
        assert!(!is_sha256(&"g".repeat(64)));
    }
}
