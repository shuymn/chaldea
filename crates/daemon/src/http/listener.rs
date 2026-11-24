//! The accept loop, `ConnIo`, and the per-connection hyper builder (design §2.2 "Accept
//! loop", "ConnIo").
//!
//! Its own loop instead of `axum::serve`, which sets no timer (so no header read timeout)
//! and no connection bound. HTTP/1.1 (HTTP/1.0 requests get the same rules), one request
//! per connection.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;

use super::guard::OuterService;
use super::registry::TaskKind;
use super::{MAX_HEADERS, MonotonicClock, Shared};

/// Accept errors that concern one connection only; the loop goes on at once (as axum's).
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// The accept loop, run as the registered `Accept` task: it owns the listener, so the
/// listener is released before the task's entry leaves the registry.
pub(crate) async fn accept_loop(shared: Arc<Shared>, listener: TcpListener, router: Router) {
    let permits = Arc::new(Semaphore::new(shared.cfg.max_connections));
    let mut shutdown = shared.shutdown_signal();
    loop {
        // Excess clients wait in the kernel backlog while no permit is free.
        let permit = tokio::select! {
            biased;
            () = shutdown.wait() => break,
            p = permits.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            biased;
            () = shutdown.wait() => break,
            r = async {
                #[cfg(test)]
                if let Some(e) = shared.faults.accept_error() {
                    return Err(e);
                }
                listener.accept().await.map(|(s, _)| s)
            } => r,
        };
        let stream = match accepted {
            Ok(s) => s,
            Err(e) if is_connection_error(&e) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed; retrying in 1 s");
                tokio::select! {
                    biased;
                    () = shutdown.wait() => break,
                    () = accept_backoff() => continue,
                }
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(error = %e, "set_nodelay failed; serving the connection anyway");
        }
        let conn = serve_connection(shared.clone(), stream, permit, router.clone());
        // Refused once closing: the stream is dropped with the unpolled future.
        if shared.registry.spawn(TaskKind::Connection, conn).is_err() {
            break;
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "accept-error backoff; shutdown wins (http-surface §2.2)"
)]
fn accept_backoff() -> Sleep {
    tokio::time::sleep(Duration::from_secs(1))
}

#[allow(
    clippy::disallowed_methods,
    reason = "the write stall, progress first: polled only after a pending write (http-surface §2.2)"
)]
fn stall_timer(stall: Duration) -> Sleep {
    tokio::time::sleep(stall)
}

/// One connection: served until its response is sent (hyper closes it after a response
/// carrying `Connection: close`), the client closes, a timeout ends it, or shutdown.
async fn serve_connection(
    shared: Arc<Shared>,
    stream: TcpStream,
    permit: OwnedSemaphorePermit,
    router: Router,
) {
    let first_byte = Arc::new(OnceLock::new());
    let conn = ConnIo::new(
        stream,
        first_byte.clone(),
        shared.cfg.clock.clone(),
        shared.cfg.write_stall_timeout,
    );
    #[cfg(test)]
    let conn = conn.observed(shared.clone());
    let io = TokioIo::new(conn);
    let service = TowerToHyperService::new(OuterService::new(shared.clone(), router, first_byte));
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(shared.cfg.header_read_timeout)
        .max_header_size(shared.cfg.max_header_bytes)
        .max_headers(MAX_HEADERS)
        // Kept on: disabling it would append `close` to a `101` (design §2.2).
        .keep_alive(true);
    let conn = builder.serve_connection(io, service).with_upgrades();
    tokio::pin!(conn);
    let mut shutdown = shared.shutdown_signal();
    tokio::select! {
        _ = conn.as_mut() => {}
        () = shutdown.wait() => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
    drop(permit);
}

/// The connection's transport: records the request origin (the first read that returns
/// bytes, on the server's clock) and fails a write that accepts no byte for `stall` (design
/// §2.2 "ConnIo").
pub(crate) struct ConnIo<T> {
    io: T,
    first_byte: Arc<OnceLock<Instant>>,
    clock: Arc<dyn MonotonicClock>,
    stall: Duration,
    /// Armed while a write is pending with no byte accepted since it started.
    timer: Option<Pin<Box<Sleep>>>,
    /// Told when the first byte is read.
    #[cfg(test)]
    shared: Option<Arc<Shared>>,
}

impl<T> ConnIo<T> {
    pub(crate) fn new(
        io: T,
        first_byte: Arc<OnceLock<Instant>>,
        clock: Arc<dyn MonotonicClock>,
        stall: Duration,
    ) -> Self {
        ConnIo {
            io,
            first_byte,
            clock,
            stall,
            timer: None,
            #[cfg(test)]
            shared: None,
        }
    }

    /// Tells `shared`'s fault points when the first byte is read.
    #[cfg(test)]
    pub(crate) fn observed(mut self, shared: Arc<Shared>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// After the inner write returned `Pending`: arms the stall timer if needed and fails
    /// the write when it expires.
    fn stalled<R>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<R>> {
        let stall = self.stall;
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(stall_timer(stall)));
        ready!(timer.as_mut().poll(cx));
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "write stalled: no byte accepted within the write stall timeout",
        )))
    }

    fn progressed<R>(
        &mut self,
        r: Poll<io::Result<R>>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<R>> {
        match r {
            Poll::Pending => self.stalled(cx),
            ready @ Poll::Ready(_) => {
                self.timer = None;
                ready
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ConnIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.io).poll_read(cx, buf);
        if matches!(r, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            #[cfg(test)]
            let fresh = self.first_byte.get().is_none();
            let clock = &self.clock;
            self.first_byte.get_or_init(|| clock.now());
            #[cfg(test)]
            if fresh && let Some(s) = &self.shared {
                s.faults.first_byte_read();
            }
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ConnIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write(cx, buf);
        self.progressed(r, cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write_vectored(cx, bufs);
        self.progressed(r, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = Pin::new(&mut self.io).poll_flush(cx);
        self.progressed(r, cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
