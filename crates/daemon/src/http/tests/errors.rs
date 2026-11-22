//! The write mapping and fatal reporting of `run_write` (design §2.8, §2.9; the plain-write
//! cases of PR7-CL-20).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use chaldea_core::http::ErrorCode;

use super::shared;
use super::support::http::TestServer;
use super::support::{FixedClock, T0, TestStore, config};
use crate::http::ctx::await_write;
use crate::http::error::{WriteFailure, refused, write_failure};
use crate::http::{ApiError, Mounts, StoreFatal};
use crate::store::test_faults::{CommitFault, Faults, open_with};
use crate::store::{JobError, StoreError, SubmitError, WriteError};

#[test]
fn write_outcomes_map_to_statuses() {
    use WriteFailure::{Submit, Write};
    let busy = (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Busy);
    let unknown = (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::OutcomeUnknown);
    let internal = (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal);
    let failed = (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::StoreFailed);
    let other = || StoreError::Constraint("x".into());
    let cases: Vec<(WriteFailure, (StatusCode, ErrorCode))> = vec![
        (Submit(SubmitError::QueueFull), busy),
        (
            Submit(SubmitError::Closed),
            (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::ShuttingDown),
        ),
        (Submit(SubmitError::Failed), failed),
        (
            Write(WriteError::Expired),
            (StatusCode::REQUEST_TIMEOUT, ErrorCode::RequestTimeout),
        ),
        (Write(WriteError::Busy), busy),
        (Write(WriteError::RolledBack(StoreError::Busy)), busy),
        (
            Write(WriteError::RolledBack(StoreError::DiskFull)),
            (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::DiskFull),
        ),
        (Write(WriteError::RolledBack(other())), internal),
        (
            Write(WriteError::Rejected(JobError::Store(other()))),
            internal,
        ),
        (
            Write(WriteError::Rejected(refused(ApiError::forbidden()))),
            (StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        ),
        (
            Write(WriteError::Rejected(JobError::Refused(
                "not an ApiError".into(),
            ))),
            internal,
        ),
        (Write(WriteError::Panicked), internal),
        (Write(WriteError::WouldDeadlock), internal),
        (Write(WriteError::Failed(other())), failed),
        (Write(WriteError::CommitUnknown(other())), unknown),
        (Write(WriteError::WriterGone), unknown),
    ];
    for (f, (status, code)) in cases {
        let what = format!("{f:?}");
        let e = write_failure(f);
        assert_eq!((e.status(), e.code()), (status, code), "{what}");
    }
    assert_eq!(ApiError::busy().retry_after(), Some(1));
    assert_eq!(ApiError::outcome_unknown().retry_after(), Some(1));
}

/// A commit whose outcome is unknown reaches the lifecycle sink, whether or not anyone
/// waits; the writer then refuses submissions, which is reported as `Failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fatal_outcomes_reach_the_sink() {
    let faults = Faults::default();
    *faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let dir = tempfile::tempdir().unwrap();
    let clock = FixedClock::new(T0);
    let store = open_with(config(dir.path().join("chaldea.db"), clock.clone()), faults).unwrap();
    let ts = TestStore {
        store: Some(store),
        clock,
        dir,
    };
    let srv = TestServer::start_on(ts, Mounts::mvp(), |_| {}).await;
    let s = shared(&srv);
    let deadline = Instant::now() + Duration::from_secs(5);
    let r = await_write(&s, deadline, Box::new(|_tx| Ok(())), None).await;
    assert_eq!(r, Err(ApiError::outcome_unknown()));
    let r = await_write(&s, deadline, Box::new(|_tx| Ok(())), None).await;
    assert_eq!(r, Err(ApiError::store_failed()));
    assert_eq!(
        *srv.sink.0.lock().unwrap(),
        [StoreFatal::CommitUnknown, StoreFatal::Failed]
    );
    // A commit nobody waits for is still reported: the observer owns the ticket.
    let srv2 = {
        let faults = Faults::default();
        *faults.commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
        let dir = tempfile::tempdir().unwrap();
        let clock = FixedClock::new(T0);
        let store =
            open_with(config(dir.path().join("chaldea.db"), clock.clone()), faults).unwrap();
        TestServer::start_on(
            TestStore {
                store: Some(store),
                clock,
                dir,
            },
            Mounts::mvp(),
            |_| {},
        )
        .await
    };
    let s2 = shared(&srv2);
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let rx = crate::http::ctx::observe_write(
        &s2,
        Instant::now() + Duration::from_secs(5),
        Box::new(move |_tx| {
            let _ = hold_rx.recv_timeout(Duration::from_secs(10));
            Ok(())
        }),
        None,
    )
    .unwrap();
    drop(rx);
    hold_tx.send(()).unwrap();
    let sink = srv2.sink.clone();
    super::eventually(Duration::from_secs(10), "the sink is told", move || {
        !sink.0.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(*srv2.sink.0.lock().unwrap(), [StoreFatal::CommitUnknown]);
    let _ = Arc::clone(&s2);
    srv.stop().await;
    srv2.stop().await;
}
