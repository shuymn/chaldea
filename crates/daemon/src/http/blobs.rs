//! `GET /api/blobs/{sha256}?stream_id=` (design §2.7): one read decides, in order, the
//! readable stream, the `blob_refs` or `large_blob_refs` row, the admission of a blob slot,
//! and the bytes (a blob in `blobs`) or the size and part count (a large blob). The slot's
//! permit moves into the response body and is freed with it. A large blob's body reads each
//! immutable part in its own short read, bounded by its own deadline and by the server's
//! shutdown, so across network waits it holds the slot and one part, never a reader
//! connection.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use chaldea_core::auth::Action;
use hyper::body::{Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::Shared;
use super::ctx::{ReadCtx, read_until};
use super::error::ApiError;
use super::guard::path_param;
use super::scope::ReadScope;
use super::sync_api::{params, store, stream_id};
use crate::store::{BlobDownload, LargeBlob};

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

/// Why a large blob's body ended before its last byte: the response is aborted, never
/// ended as if complete.
#[derive(Debug)]
struct PartError(String);

impl fmt::Display for PartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "large blob body: {}", self.0)
    }
}

impl std::error::Error for PartError {}

/// One part's read: its bytes, or why the body must fail.
type PartRead = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>>;

/// Reads part `index` in one short read, cut off at the request deadline's length from now
/// (a read not returned by then fails the body), or ended at once by the server's shutdown.
fn read_part(shared: Arc<Shared>, sha: Arc<str>, index: u64) -> PartRead {
    let mut shutdown = shared.shutdown_signal();
    Box::pin(async move {
        let deadline = shared.cfg.clock.now() + shared.cfg.short_deadline;
        let read = read_until(&shared, deadline, move |tx| tx.large_blob_part(&sha, index));
        tokio::select! {
            biased;
            () = shutdown.wait() => Err("the server is shutting down".to_owned()),
            r = read => match r {
                Ok(Some(data)) => Ok(data),
                Ok(None) => Err(format!("part {index} is missing")),
                Err(e) => Err(format!("part {index}: {e:?}")),
            },
        }
    })
}

/// A large blob's body: its parts in order, each fetched by its own short read
/// ([`read_part`]) when the previous one is sent. It holds the slot, at most one part, and at
/// most one read in flight; dropping it releases the slot (a read in flight finishes on its
/// own).
struct PartsBody {
    shared: Arc<Shared>,
    sha: Arc<str>,
    part_count: u64,
    /// The next part to fetch.
    next: u64,
    /// Bytes not yet sent.
    left: u64,
    /// The unsent bytes of the current part.
    buf: Bytes,
    pending: Option<PartRead>,
    failed: bool,
    _permit: OwnedSemaphorePermit,
}

impl PartsBody {
    fn new(shared: Arc<Shared>, sha: &str, blob: &LargeBlob, permit: OwnedSemaphorePermit) -> Self {
        PartsBody {
            shared,
            sha: sha.into(),
            part_count: blob.part_count,
            next: 0,
            left: blob.size,
            buf: Bytes::new(),
            pending: None,
            failed: false,
            _permit: permit,
        }
    }

    fn fail(&mut self, why: String) -> Poll<Option<Result<Frame<Bytes>, PartError>>> {
        #[cfg(test)]
        self.shared
            .faults
            .blob_body_errors
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.failed = true;
        self.pending = None;
        self.buf = Bytes::new();
        Poll::Ready(Some(Err(PartError(why))))
    }
}

impl hyper::body::Body for PartsBody {
    type Data = Bytes;
    type Error = PartError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        loop {
            if self.failed {
                return Poll::Ready(None);
            }
            if !self.buf.is_empty() {
                let n = self.buf.len().min(FRAME_BYTES);
                let chunk = self.buf.split_to(n);
                self.left -= n as u64;
                return Poll::Ready(Some(Ok(Frame::data(chunk))));
            }
            if self.next == self.part_count {
                if self.left == 0 {
                    return Poll::Ready(None);
                }
                let left = self.left;
                return self.fail(format!("the parts end {left} bytes short"));
            }
            if self.pending.is_none() {
                let read = read_part(self.shared.clone(), self.sha.clone(), self.next);
                self.pending = Some(read);
            }
            let Some(pending) = self.pending.as_mut() else {
                continue;
            };
            let read = ready!(pending.as_mut().poll(cx));
            self.pending = None;
            let index = self.next;
            match read {
                Ok(data) if !data.is_empty() && data.len() as u64 <= self.left => {
                    self.next += 1;
                    self.buf = Bytes::from(data);
                }
                Ok(data) => {
                    return self.fail(format!("part {index} has {} bytes", data.len()));
                }
                Err(why) => return self.fail(why),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed || (self.left == 0 && self.buf.is_empty() && self.next == self.part_count)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
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
    let sha_read = sha.clone();
    let (download, permit) = ctx
        .read(move |tx, auth| {
            // 1. A readable stream.
            let scope = ReadScope::of(tx, auth, Action::ReadBlob).map_err(store)?;
            if !scope.readable_stream(tx, stream).map_err(store)? {
                return Err(ApiError::not_found());
            }
            // 2. A reference from that stream; the same 404 whether or not a slot is free.
            if !tx.blob_ref_exists(stream, &sha_read).map_err(store)? {
                return Err(ApiError::not_found());
            }
            // 3. Admission, non-blocking.
            let permit = slots.try_acquire_owned().map_err(|_| ApiError::busy())?;
            // 4. The bytes, or a large blob's size and part count, in the same snapshot as
            // the reference.
            let download = tx
                .blob_download(stream, &sha_read)
                .map_err(store)?
                .ok_or_else(ApiError::not_found)?;
            Ok((download, permit))
        })
        .await?;
    let (len, ty, body) = match download {
        BlobDownload::Whole(blob) => (
            blob.data.len() as u64,
            download_type(&blob.media_type),
            Body::new(BlobBody {
                data: blob.data,
                sent: 0,
                _permit: permit,
            }),
        ),
        BlobDownload::Parts(large) => (
            large.size,
            download_type(&large.media_type),
            Body::new(PartsBody::new(ctx.shared().clone(), &sha, &large, permit)),
        ),
    };
    let mut resp = Response::new(body);
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
    use crate::http::Mounts;
    use crate::http::tests::shared;
    use crate::http::tests::support::http::TestServer;
    use crate::store::MAX_BLOB_BYTES;
    use crate::store::tests::support::waits;
    use chaldea_core::ids::StreamId;
    use chaldea_core::payload::Content;
    use http_body_util::BodyExt;
    use std::time::Duration;

    /// Stores a value of `MAX_BLOB_BYTES + 1` bytes on the host stream; returns its hash
    /// and the bytes.
    async fn put_large(srv: &TestServer) -> (String, Vec<u8>) {
        let bytes: Vec<u8> = (0..=MAX_BLOB_BYTES)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let mut spool = srv.store.store().blob_spool().unwrap();
        spool.append(&bytes).unwrap();
        let p = spool.finish("application/octet-stream").unwrap();
        let Content::Blob { blob, .. } = &p.content else {
            panic!("a blob")
        };
        let sha = blob.sha256.clone();
        let mut e =
            crate::http::tests::support::raw_unknown(StreamId::Host, "k", p.content.clone());
        e.blobs = p.blob.into_iter().collect();
        srv.append_within(vec![e], Duration::from_secs(300)).await;
        (sha, bytes)
    }

    /// The body over the stored blob, with one slot of `slots`.
    fn body(srv: &TestServer, sha: &str, slots: &Arc<Semaphore>) -> PartsBody {
        let s = shared(srv);
        let large = srv
            .store
            .read({
                let sha = sha.to_owned();
                move |tx| tx.blob_download(StreamId::Host, &sha)
            })
            .unwrap();
        let BlobDownload::Parts(large) = large else {
            panic!("a large blob")
        };
        PartsBody::new(s, sha, &large, slots.clone().try_acquire_owned().unwrap())
    }

    /// Polls frames until the body ends or fails; returns the bytes and whether it failed.
    async fn drain(mut b: PartsBody, limit: usize) -> (Vec<u8>, Option<String>, PartsBody) {
        let mut got = Vec::new();
        while got.len() < limit {
            match b.frame().await {
                Some(Ok(f)) => got.extend_from_slice(f.data_ref().unwrap()),
                Some(Err(e)) => return (got, Some(e.to_string()), b),
                None => break,
            }
        }
        (got, None, b)
    }

    /// A body ends cleanly only after its last byte; a missing part is an error, never an
    /// early end; dropping the body releases its slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parts_body_fails_instead_of_ending_short() {
        let srv = TestServer::start(Mounts::mvp()).await;
        let (sha, bytes) = put_large(&srv).await;
        let slots = Arc::new(Semaphore::new(1));
        let (got, err, b) = drain(body(&srv, &sha, &slots), usize::MAX).await;
        assert_eq!(err, None);
        assert!(got == bytes, "the body's bytes differ");
        assert!(hyper::body::Body::is_end_stream(&b));
        assert_eq!(slots.available_permits(), 0, "the body holds its slot");
        drop(b);
        assert_eq!(
            slots.available_permits(),
            1,
            "dropping the body frees its slot"
        );

        let c = rusqlite::Connection::open(srv.store.path()).unwrap();
        c.execute_batch(&format!(
            "DROP TRIGGER large_blob_parts_no_delete;
             DELETE FROM large_blob_parts WHERE sha256 = '{sha}' AND part_index = 16;"
        ))
        .unwrap();
        drop(c);
        let (got, err, mut b) = drain(body(&srv, &sha, &slots), usize::MAX).await;
        assert_eq!(got.len(), 16 * 4 * 1024 * 1024);
        assert!(err.is_some_and(|e| e.contains("part 16 is missing")));
        assert!(b.frame().await.is_none(), "nothing after the error");
        drop(b);
        srv.stop().await;
    }

    /// A store shut down mid-body fails the body at its next part.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parts_body_fails_when_the_store_closes() {
        let mut srv = TestServer::start(Mounts::mvp()).await;
        let (sha, _) = put_large(&srv).await;
        let slots = Arc::new(Semaphore::new(1));
        let (got, err, b) = drain(body(&srv, &sha, &slots), 1).await;
        assert_eq!((got.len(), err), (FRAME_BYTES, None));
        srv.store.shutdown();
        let (_, err, b) = drain(b, usize::MAX).await;
        assert!(
            err.is_some_and(|e| e.contains("part 1")),
            "the next part fails"
        );
        drop(b);
        srv.shutdown_http(std::time::Instant::now() + std::time::Duration::from_secs(10))
            .await;
    }

    /// Holds the next store read (part 0's) until the returned sender drops; `entered` is
    /// told once the read is held.
    fn hold_next_read(
        srv: &TestServer,
    ) -> (
        std::sync::mpsc::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        shared(srv).faults.arm_before_read(0, move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
        });
        (release_tx, entered_rx)
    }

    /// A part read that has not returned by its deadline fails the body; it does not wait
    /// for the read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parts_body_fails_when_a_read_outlasts_its_deadline() {
        let srv = TestServer::start_with(Mounts::mvp(), |c| {
            c.short_deadline = Duration::from_millis(300);
        })
        .await;
        let (sha, _) = put_large(&srv).await;
        let slots = Arc::new(Semaphore::new(1));
        let (release, _entered) = hold_next_read(&srv);
        let (got, err, b) = waits::bounded(
            Duration::from_secs(10),
            drain(body(&srv, &sha, &slots), usize::MAX),
        )
        .await
        .expect("the body failed while its read was held");
        assert!(got.is_empty());
        assert!(err.is_some_and(|e| e.contains("part 0")), "part 0 fails");
        drop(b);
        drop(release);
        srv.stop().await;
    }

    /// The server's shutdown fails a body whose part read is in flight, long before the
    /// read's deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parts_body_fails_on_server_shutdown() {
        let srv = TestServer::start_with(Mounts::mvp(), |c| {
            c.short_deadline = Duration::from_secs(60);
        })
        .await;
        let (sha, _) = put_large(&srv).await;
        let slots = Arc::new(Semaphore::new(1));
        let (release, entered) = hold_next_read(&srv);
        let draining = tokio::spawn(drain(body(&srv, &sha, &slots), usize::MAX));
        waits::bounded(Duration::from_secs(10), entered)
            .await
            .expect("the part read started")
            .unwrap();
        shared(&srv).shutdown.send_replace(true);
        let (got, err, b) = waits::bounded(Duration::from_secs(10), draining)
            .await
            .expect("the body failed at the shutdown")
            .unwrap();
        assert!(got.is_empty());
        assert!(
            err.is_some_and(|e| e.contains("shutting down")),
            "the shutdown ends it"
        );
        drop(b);
        drop(release);
        srv.stop().await;
    }

    /// Over a real connection, a part missing mid-stream aborts the response: the client
    /// gets a prefix shorter than `Content-Length`, and the body ended with an error, not an
    /// early end that hyper would also cut short.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_part_aborts_a_real_response() {
        let srv = TestServer::start(Mounts::mvp()).await;
        let owner = srv
            .token(chaldea_core::auth::TokenKind::Owner, None, None)
            .await;
        let (sha, bytes) = put_large(&srv).await;
        let c = rusqlite::Connection::open(srv.store.path()).unwrap();
        c.execute_batch(&format!(
            "DROP TRIGGER large_blob_parts_no_delete;
             DELETE FROM large_blob_parts WHERE sha256 = '{sha}' AND part_index = 1;"
        ))
        .unwrap();
        drop(c);
        let r = srv
            .send(
                &srv.req("GET", &format!("/api/blobs/{sha}?stream_id=host"))
                    .bearer(&owner),
            )
            .await;
        assert_eq!(r.status, 200);
        let len = bytes.len().to_string();
        assert_eq!(r.header("content-length"), Some(len.as_str()));
        assert!(
            r.body.len() <= 4 * 1024 * 1024,
            "{} bytes past part 0",
            r.body.len()
        );
        assert!(r.body == bytes[..r.body.len()], "the bytes sent differ");
        assert_eq!(
            shared(&srv)
                .faults
                .blob_body_errors
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the body ended with an error"
        );
        srv.stop().await;
    }

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
