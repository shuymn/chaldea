//! The tick (design §2.6): every second, reap the git slot's `left` child, then per live
//! candidate a job that re-validates everything: `session_start_timeout` and
//! `buffer::expire`; registered [`TickStep`]s run after 10a1's.

use std::sync::Arc;
use std::time::Instant;

use chaldea_core::payload::SourceKind;
use chaldea_core::state::{AttentionCause, ProcessState, StateInput};

use super::events::{ThreadJob, derived, launch_key};
use super::live::LiveLaunch;
use super::record::{Job, record};
use super::{SESSION_START_TIMEOUT, Supervisor, TICK};

/// A step 10b adds to every tick (its gate timeout and ack window).
pub(crate) trait TickStep: Send + Sync {
    fn tick(&self, now: Instant, live: &[Arc<LiveLaunch>]) -> Vec<Job>;
}

/// The tick's period timer: the supervisor's exception to the timer rule (deadlines.md);
/// it decides nothing, each tick re-reads the store.
#[allow(
    clippy::disallowed_methods,
    reason = "the supervisor tick's period (design §2.6); each tick decides from the store"
)]
pub(crate) fn tick_timer(at: tokio::time::Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(at)
}

impl Supervisor {
    /// One tick at `now` (the seam tests drive).
    pub(crate) async fn tick(self: &Arc<Self>, now: Instant) {
        self.git.reap_left();
        let live = self.live.all();
        let mut jobs: Vec<Job> = Vec::new();
        for l in &live {
            if !l.manual
                && let Some(c) = l.committed()
                && now >= c.at + SESSION_START_TIMEOUT
            {
                jobs.push(session_start_timeout(l));
            }
            jobs.push(expire_buffer(self, l));
        }
        for s in &self.cfg.tick_steps {
            jobs.extend(s.tick(now, &live));
        }
        for j in jobs {
            let job: super::record::JobOf<()> = j;
            let _ = record(self, job, false).await;
        }
    }

    /// The tick task: one tick per [`TICK`] until admission closes.
    pub(crate) async fn tick_loop(self: Arc<Self>) {
        let mut next = self.now();
        loop {
            next += TICK;
            tick_timer(self.timer(next)).await;
            if !self.tracker.accepting() {
                return;
            }
            self.tick(self.now()).await;
        }
    }
}

/// Raises `session_start_timeout` only for the thread's current, started, live, unbound
/// launch whose cause is absent (design §2.6).
fn session_start_timeout(l: &LiveLaunch) -> Job {
    let (thread, launch) = (l.thread_id, l.launch_id);
    let pid = l.committed().map(|c| c.identity.pid);
    Arc::new(move |tx| {
        let r = tx.read();
        let (Some(t), Some(row)) = (r.thread(thread)?, r.launch(launch)?) else {
            return Ok(());
        };
        drop(r);
        let alive = matches!(t.state.process, ProcessState::Running | ProcessState::Stopping);
        let cause = AttentionCause::SessionStartTimeout;
        if t.current_launch_id != Some(launch)
            || row.pid.is_none()
            || row.pid != pid
            || row.exit_reason.is_some()
            || !alive
            || row.session_id.is_some()
            || t.state.attention.contains(&cause)
        {
            return Ok(());
        }
        let mut job = ThreadJob::new(thread, t.agent, t.state, SourceKind::Daemon, None);
        let key = derived(&launch_key(launch, "started"), "session_start_timeout");
        job.apply(&key, "raise", &StateInput::AttentionRaised(cause), Some(launch))?;
        job.flush(tx)?;
        Ok(())
    })
}

/// `buffer::expire` for a live, unbound launch.
fn expire_buffer(sup: &Supervisor, l: &LiveLaunch) -> Job {
    let launch = l.launch_id;
    let cfg = sup.cfg.hook.clone();
    Arc::new(move |tx| {
        let Some(row) = tx.read().launch(launch)? else {
            return Ok(());
        };
        if row.exit_reason.is_some() || row.session_id.is_some() {
            return Ok(());
        }
        crate::hook::expire(tx, launch, &cfg)?;
        Ok(())
    })
}
