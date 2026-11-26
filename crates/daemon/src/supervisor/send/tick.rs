//! `SendTick` (send §2.7): forwards each tick to the driver; it returns no job.

use std::sync::Arc;
use std::time::Instant;

use super::super::live::LiveLaunch;
use super::super::record::Job;
use super::super::tick::TickStep;
use super::SendSide;

pub(crate) struct SendTick(pub(crate) Arc<SendSide>);

impl TickStep for SendTick {
    fn tick(&self, now: Instant, _live: &[Arc<LiveLaunch>]) -> Vec<Job> {
        // Never blocks: the watch keeps the latest value.
        self.0.ticks.send_replace(Some(now));
        Vec::new()
    }
}
