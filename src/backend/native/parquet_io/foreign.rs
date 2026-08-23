//! Foreign flat dtypes → their string forms (ADR 0034's totality, ADR 0033
//! Stage 2): a parquet column the engine has no native dtype for still READS —
//! as the value's text — rather than erroring a whole file for one column. The
//! formats mirror what the polars bridge displays today (ISO dates, naive
//! `%Y-%m-%d %H:%M:%S[.frac]` timestamps, full-scale decimals), so the Stage-4
//! flip does not change what programs see. All date math is Hinnant's civil
//! calendar — no chrono dependency.

pub use crate::backend::timefmt::{civil_from_days, timestamp_str};

/// A DATE column's cell: days since epoch → `2026-08-23`.
pub fn date_str(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}


/// A TIME cell: value in the unit since midnight → `HH:MM:SS[.frac]`.
pub fn time_str(v: i64, unit_per_sec: i64, width: usize) -> String {
    let secs = v.div_euclid(unit_per_sec);
    let frac = v.rem_euclid(unit_per_sec);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if frac == 0 {
        format!("{hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{hh:02}:{mm:02}:{ss:02}.{frac:0width$}")
    }
}

/// A DECIMAL cell: unscaled integer + scale → full-scale text (`123.45`,
/// `1.00`, `-0.05`) — no zero-trimming, the bridge's display.
pub fn decimal_str(unscaled: i128, scale: i32) -> String {
    if scale <= 0 {
        return unscaled.to_string();
    }
    let scale = scale as u32;
    let sign = if unscaled < 0 { "-" } else { "" };
    let mag = unscaled.unsigned_abs();
    let pow = 10u128.pow(scale);
    format!("{sign}{}.{:0width$}", mag / pow, mag % pow, width = scale as usize)
}

/// Big-endian two's-complement bytes (a decimal's BYTE_ARRAY / FLBA payload) →
/// i128, sign-extended.
pub fn be_bytes_to_i128(bytes: &[u8]) -> i128 {
    let mut acc: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) { -1 } else { 0 };
    for &b in bytes {
        acc = (acc << 8) | b as i128;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_exact() {
        assert_eq!(date_str(0), "1970-01-01");
        assert_eq!(date_str(20688), "2026-08-23");
        assert_eq!(date_str(-1), "1969-12-31");
    }

    #[test]
    fn timestamps_show_the_fraction_only_when_nonzero() {
        assert_eq!(timestamp_str(0, 1_000, 3), "1970-01-01 00:00:00");
        assert_eq!(timestamp_str(1_787_443_200_123, 1_000, 3), "2026-08-23 00:00:00.123");
        assert_eq!(timestamp_str(-1, 1_000, 3), "1969-12-31 23:59:59.999");
    }

    #[test]
    fn decimals_keep_their_scale() {
        assert_eq!(decimal_str(12345, 2), "123.45");
        assert_eq!(decimal_str(100, 2), "1.00");
        assert_eq!(decimal_str(-5, 2), "-0.05");
        assert_eq!(decimal_str(7, 0), "7");
        assert_eq!(be_bytes_to_i128(&[0xFF, 0xFB]), -5);
        assert_eq!(be_bytes_to_i128(&[0x30, 0x39]), 12345);
    }
}
