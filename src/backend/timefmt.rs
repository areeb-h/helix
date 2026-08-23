//! Shared instant formatting — Hinnant's civil calendar, no chrono, no timezone
//! database. Both DataFrame backends lean on it: the native engine's parquet
//! reader renders foreign temporal dtypes with it, and the polars bridge uses it
//! to render tz-aware datetimes that polars' own Display PANICS on without its
//! `timezones` feature (an ADR 0024 hazard reproduced from a foreign parquet
//! file). A tz-aware value IS a UTC instant; rendering it as UTC text with the
//! ` UTC` suffix is truthful without carrying a timezone database, and both
//! engines produce the identical bytes.

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's civil_from_days.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A naive timestamp: value in the unit → `%Y-%m-%d %H:%M:%S`, plus the
/// fractional group at the unit's width only when nonzero (the polars bridge's
/// display convention). `unit_per_sec` ∈ {1e3, 1e6, 1e9}; `width` ∈ {3, 6, 9}.
pub fn timestamp_str(v: i64, unit_per_sec: i64, width: usize) -> String {
    let secs = v.div_euclid(unit_per_sec);
    let frac = v.rem_euclid(unit_per_sec);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    if frac == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{frac:0width$}")
    }
}
