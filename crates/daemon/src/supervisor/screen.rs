//! `GET /api/threads/{thread_id}/screen` (C1 D1): the live screen of a thread's current
//! launch, for diagnosis. It reads; it appends no event.

use std::sync::Weak;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use chaldea_core::http::ErrorCode;
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_core::payload::{VtCursor, VtModes};
use chaldea_core::sync::ScreenResponse;
use futures_util::future::BoxFuture;

use super::Supervisor;
use super::live::Snapshot;
use crate::http::{
    ApiError, Mount, ReadCtx, ReadHandler, json, no_query, readable_summary, spec, thread_in_path,
};

/// The route's path.
pub(crate) const SCREEN_PATH: &str = "/api/threads/{thread_id}/screen";

/// The longest a snapshot waits for the VT lock and the blocking pool.
const SNAPSHOT_WAIT: Duration = Duration::from_millis(250);

/// The snapshot's deadline: [`SNAPSHOT_WAIT`] from `now`, never past the request's.
fn snapshot_deadline(now: Instant, request: Instant) -> Instant {
    (now + SNAPSHOT_WAIT).min(request)
}

/// The response for a snapshot of launch `l` of thread `t`.
fn response(t: ThreadId, l: LaunchId, s: &chaldea_pty_runtime::Snapshot) -> ScreenResponse {
    ScreenResponse::new(
        t,
        l,
        s.cols,
        s.rows,
        VtCursor {
            row: s.cursor.row,
            col: s.cursor.col,
            visible: s.cursor.visible,
        },
        VtModes {
            bracketed_paste: s.modes.bracketed_paste,
            alternate_screen: s.modes.alt_screen,
        },
        s.lines
            .iter()
            .map(chaldea_pty_runtime::Line::text)
            .collect(),
    )
}

struct ReadScreen(Weak<Supervisor>);

impl ReadHandler for ReadScreen {
    fn call(
        &self,
        ctx: ReadCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response<Body>, ApiError>> {
        let sup = self.0.clone();
        Box::pin(async move {
            no_query(&req)?;
            let t = thread_in_path(SCREEN_PATH, &req)?;
            // The thread read check before any live lookup.
            let summary = readable_summary(&ctx, t).await?;
            let no_live = || ApiError::new(StatusCode::CONFLICT, ErrorCode::NoLiveLaunch);
            let sup = sup.upgrade().ok_or_else(ApiError::shutting_down)?;
            // A launch whose exit is recorded has no screen, even while its session is still
            // in its slot.
            let entry = summary
                .launch
                .filter(|l| l.exit_reason.is_none())
                .and_then(|l| sup.live(t, l.launch_id))
                .ok_or_else(no_live)?;
            let deadline = snapshot_deadline(sup.now(), ctx.deadline());
            // A busy answer taken at the request's deadline is the guard's 408.
            match entry.snapshot_before(&sup, deadline).await {
                Snapshot::Taken(s) => json(&response(t, entry.launch_id, &s)),
                Snapshot::NoSession => Err(no_live()),
                Snapshot::Busy => Err(ApiError::busy()),
                Snapshot::VtFailed => Err(ApiError::internal()),
            }
        })
    }
}

/// The route's mount over `sup`.
pub(crate) fn mount(sup: Weak<Supervisor>) -> Mount {
    let route =
        spec(&Method::GET, SCREEN_PATH).unwrap_or_else(|| panic!("GET {SCREEN_PATH} is in ROUTES"));
    Mount::read(route, ReadScreen(sup))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C1-11: the snapshot never waits past the request's deadline.
    #[test]
    fn deadline_is_clamped() {
        let now = Instant::now();
        let late = now + Duration::from_secs(5);
        assert_eq!(snapshot_deadline(now, late), now + SNAPSHOT_WAIT);
        let near = now + Duration::from_millis(100);
        assert_eq!(snapshot_deadline(now, near), near);
    }
}
