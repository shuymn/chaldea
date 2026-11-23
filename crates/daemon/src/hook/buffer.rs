//! The buffer of an unbound launch (`docs/design/hook-ingest.md` §2.9): its meta format,
//! bounds, expiry, and `resolve_unbindable`. A launch's buffer is its deliveries with
//! routing `buffered`; nothing else holds it.

use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_core::payload::RawUnknownReason;
use serde::{Deserialize, Serialize};

use super::HookConfig;
use super::ancestry::Link;
use super::intake::IntakeFacts;
use super::route::{JSON, raw_event};
use crate::store::{BufferedDelivery, BufferedSummary, Promotion, StoreError, WriteTx};

/// `BufferedHook.meta`, version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Meta {
    pub(crate) v: u32,
    pub(crate) thread_header: Option<ThreadId>,
    pub(crate) agent_header: String,
    pub(crate) emitter_version: String,
    pub(crate) attempt: Option<u8>,
    pub(crate) chain: Vec<Link>,
    pub(crate) chain_end: String,
    /// Present exactly when the probe returned facts, whatever their JSON (`null`
    /// included); absent for `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub(crate) intake: Option<IntakeFacts>,
}

/// A present `intake` field is `Some`, whatever its value.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<IntakeFacts>, D::Error> {
    IntakeFacts::deserialize(d).map(Some)
}

impl Meta {
    pub(crate) const VERSION: u32 = 1;

    /// The stored meta, `None` when it does not parse or is not version 1.
    pub(crate) fn parse(text: &str) -> Option<Meta> {
        serde_json::from_str::<Meta>(text)
            .ok()
            .filter(|m| m.v == Meta::VERSION)
    }
}

/// Whether adding a body of `len` bytes keeps the buffer within its bounds (§2.9 "Bounds").
pub(crate) fn fits(s: &BufferedSummary, len: u64, cfg: &HookConfig) -> bool {
    let count_ok =
        usize::try_from(s.count.saturating_add(1)).is_ok_and(|c| c <= cfg.buffer_max_count);
    count_ok && s.body_bytes.saturating_add(len) <= cfg.buffer_max_bytes
}

/// Promotes every buffered delivery of `launch` to `raw_unknown` `unbound`, in receipt
/// order. Returns how many.
fn drain_unbound(tx: &mut WriteTx<'_>, launch: LaunchId) -> Result<usize, StoreError> {
    let rows = tx.read().buffered_deliveries(launch)?;
    promote_unbound(tx, launch, rows)
}

/// Promotes `rows`, buffered deliveries of `launch` read in this job, to `raw_unknown`
/// `unbound`, in their order. Returns how many.
pub(crate) fn promote_unbound(
    tx: &mut WriteTx<'_>,
    launch: LaunchId,
    rows: Vec<BufferedDelivery>,
) -> Result<usize, StoreError> {
    let Some(l) = tx.read().launch(launch)? else {
        return Ok(0);
    };
    let n = rows.len();
    for b in rows {
        let e = raw_event(
            l.thread_id,
            &l,
            super::route::hook_key(b.emit_id),
            RawUnknownReason::Unbound,
            JSON,
            b.hook.body,
            None,
        )?;
        tx.hook_delivery_promote(launch, b.emit_id, Promotion::RawUnknown(e))?;
    }
    Ok(n)
}

/// Expires the launch's buffer when the running job's `recorded_at` is more than
/// `buffer_ttl` after the earliest `received_at` of its buffered deliveries: every one
/// becomes `raw_unknown` `unbound`, in receipt order. A negative age (the clock stepped
/// back) or a stamp that does not parse expires nothing. Returns how many expired.
///
/// # Errors
///
/// A store error.
pub fn expire(
    tx: &mut WriteTx<'_>,
    launch: LaunchId,
    cfg: &HookConfig,
) -> Result<usize, StoreError> {
    expire_in(tx, launch, cfg).map(|(n, _)| n)
}

/// [`expire`], with the buffer's summary after it: empty once it expired.
pub(crate) fn expire_in(
    tx: &mut WriteTx<'_>,
    launch: LaunchId,
    cfg: &HookConfig,
) -> Result<(usize, BufferedSummary), StoreError> {
    let summary = tx.read().buffered_summary(launch)?;
    let Some(earliest) = &summary.earliest_received_at else {
        return Ok((0, summary));
    };
    let (Some(now), Some(oldest)) = (millis(&tx.recorded_at().0), millis(&earliest.0)) else {
        return Ok((0, summary));
    };
    let ttl = i64::try_from(cfg.buffer_ttl.as_millis()).unwrap_or(i64::MAX);
    if now.saturating_sub(oldest) > ttl {
        let n = drain_unbound(tx, launch)?;
        let empty = BufferedSummary {
            count: 0,
            body_bytes: 0,
            earliest_received_at: None,
        };
        Ok((n, empty))
    } else {
        Ok((0, summary))
    }
}

/// For a launch that can no longer bind (its exit, or a restart): every buffered delivery
/// becomes `raw_unknown` `unbound`, in receipt order. PR 10 calls it in the job that
/// appends `launch_exited` and, at restart, for each of `buffered_launches()`. Returns how
/// many.
///
/// # Errors
///
/// A store error.
pub fn resolve_unbindable(tx: &mut WriteTx<'_>, launch: LaunchId) -> Result<usize, StoreError> {
    drain_unbound(tx, launch)
}

/// Milliseconds since the Unix epoch of a store stamp `YYYY-MM-DDTHH:MM:SS.mmmZ`
/// (`core::ids::Timestamp`); `None` for any other text.
pub(crate) fn millis(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    if b.len() != 24 {
        return None;
    }
    let sep = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (23, b'Z'),
    ];
    if sep.iter().any(|&(i, c)| b[i] != c) {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let s = &b[from..to];
        if !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s, ms) = (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some((((days * 24 + h) * 60 + mi) * 60 + s) * 1000 + ms)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Facts the probe returned as JSON `null` stay present through the stored meta;
    /// absent facts stay absent.
    #[test]
    fn present_null_intake_survives_buffering() {
        let mut original = Meta {
            v: Meta::VERSION,
            thread_header: None,
            agent_header: "codex".into(),
            emitter_version: "test".into(),
            attempt: Some(1),
            chain: vec![],
            chain_end: "no_pid".into(),
            intake: Some(IntakeFacts(serde_json::Value::Null)),
        };
        let stored = serde_json::to_string(&original).unwrap();
        assert_eq!(Meta::parse(&stored).unwrap().intake, original.intake);
        original.intake = None;
        let stored = serde_json::to_string(&original).unwrap();
        assert_eq!(Meta::parse(&stored).unwrap().intake, None);
    }

    /// The stamp format and its boundaries.
    #[test]
    fn timestamp() {
        assert_eq!(millis("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(millis("1970-01-01T00:00:01.001Z"), Some(1001));
        assert_eq!(millis("2000-03-01T00:00:00.000Z"), Some(951_868_800_000));
        assert_eq!(millis("2024-02-29T23:59:59.999Z"), Some(1_709_251_199_999));
        assert_eq!(
            millis("2026-11-22T00:00:30.000Z").unwrap()
                - millis("2026-11-22T00:00:00.000Z").unwrap(),
            30_000
        );
        // Across a year boundary.
        assert_eq!(
            millis("2027-01-01T00:00:00.000Z").unwrap()
                - millis("2026-12-31T23:59:59.999Z").unwrap(),
            1
        );
        for bad in [
            "",
            "2026-11-22T00:00:00Z",
            "2026-11-22T00:00:00.000",
            "2026-11-22 00:00:00.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-02-29T00:00:00.000Z",
            "2026-11-22T24:00:00.000Z",
            "2026-11-22T00:60:00.000Z",
            "2026-11-22T00:00:60.000Z",
            "2026-11-22T00:00:00.+00Z",
            "+026-11-22T00:00:00.000Z",
        ] {
            assert_eq!(millis(bad), None, "{bad}");
        }
    }
}
