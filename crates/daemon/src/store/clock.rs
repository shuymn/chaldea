//! The production clock and id source.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chaldea_core::ids::{Epoch, EventId, Timestamp};
use uuid::Uuid;

use super::{Clock, IdSource};

/// The system clock: `Instant::now` and the wall time as RFC 3339 UTC with milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> (Instant, Timestamp) {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        (Instant::now(), format_utc_ms(ms))
    }
}

/// Random v4 UUIDs for event ids and the epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl IdSource for RandomIds {
    fn event_id(&self) -> EventId {
        EventId(Uuid::new_v4())
    }

    fn epoch(&self) -> Epoch {
        Epoch(Uuid::new_v4().to_string())
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for milliseconds since the Unix epoch.
pub(crate) fn format_utc_ms(ms: u128) -> Timestamp {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    Timestamp(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    ))
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date of a day count.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::format_utc_ms;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_utc_ms(0).0, "1970-01-01T00:00:00.000Z");
        // 2026-11-22T09:45:00.123Z
        assert_eq!(
            format_utc_ms(1_795_340_700_123).0,
            "2026-11-22T09:45:00.123Z"
        );
        // A leap day.
        assert_eq!(format_utc_ms(951_782_400_000).0, "2000-02-29T00:00:00.000Z");
    }
}
