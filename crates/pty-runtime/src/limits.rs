//! Internal, configurable bounds of a session (`docs/design/pty-runtime.md` §5).

use std::time::Duration;

use chaldea_core::input::MAX_PROMPT_BYTES;

/// The worst-case keyed (`win_keys`) expansion per input byte: one LF becoming a 36-byte
/// Shift+Enter record pair.
const KEYED_EXPANSION: usize = 36;
/// Room for the padding spaces and the submit of a send unit.
const UNIT_OVERHEAD: usize = 64;

/// Per-session limits. Defaults derive from [`MAX_PROMPT_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Queued user units.
    pub queue_units: usize,
    /// Bytes of one unit.
    pub max_unit_bytes: usize,
    /// Bytes of all queued user units.
    pub queue_bytes: usize,
    /// Total pause of one unit.
    pub max_pause: Duration,
    /// Queued VT replies.
    pub reply_queue: usize,
    /// Records kept in the diagnostic ring.
    pub log_records: usize,
    /// Output subscribers.
    pub subscribers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            queue_units: 256,
            max_unit_bytes: MAX_PROMPT_BYTES * KEYED_EXPANSION + UNIT_OVERHEAD,
            queue_bytes: 16 * 1024 * 1024,
            max_pause: Duration::from_secs(5),
            reply_queue: 64,
            log_records: 4096,
            subscribers: 8,
        }
    }
}

/// The budget of each close step (§2.2 close order); `close` returns within their sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseBounds {
    /// Step 2: the tree kill.
    pub kill: Duration,
    /// Step 3: the writer releasing the input.
    pub input: Duration,
    /// Step 4: closing the PTY.
    pub close: Duration,
    /// Step 5: the drain reaching its end.
    pub drain: Duration,
    /// Step 6: joining threads and the final snapshot.
    pub join: Duration,
}

impl Default for CloseBounds {
    fn default() -> Self {
        CloseBounds {
            kill: Duration::from_secs(5),
            input: Duration::from_secs(1),
            close: Duration::from_secs(2),
            drain: Duration::from_secs(2),
            join: Duration::from_secs(1),
        }
    }
}

impl CloseBounds {
    /// The sum of every budget: the bound on `close`.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.kill + self.input + self.close + self.drain + self.join
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_design() {
        let l = Limits::default();
        assert_eq!(l.max_unit_bytes, 9_437_248);
        assert_eq!(l.queue_units, 256);
        assert_eq!(l.queue_bytes, 16 << 20);
        assert_eq!(CloseBounds::default().total(), Duration::from_secs(11));
    }
}
