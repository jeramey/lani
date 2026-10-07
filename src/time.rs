//! Timestamps as stored in the database: RFC 3339, UTC, fixed millisecond precision.
//!
//! The fixed width matters: it makes lexicographic order equal chronological order.

/// The current time, e.g. `2026-10-07T14:02:11.482Z`.
#[must_use]
pub fn now() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// A stored timestamp shortened to minutes for compact display, e.g. `2026-10-07T14:02Z`.
#[must_use]
pub fn to_minutes(timestamp: &str) -> String {
    match timestamp.get(..16) {
        Some(prefix) => format!("{prefix}Z"),
        None => timestamp.to_string(),
    }
}

/// A stored timestamp in the machine's local time zone, e.g. `2026-10-07 07:02`.
/// The web server runs as the user on their own machine, so this is the user's local time.
/// Unparseable input is returned unchanged.
#[must_use]
pub fn to_local(timestamp: &str) -> String {
    match timestamp.parse::<jiff::Timestamp>() {
        Ok(parsed) => parsed
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => timestamp.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_has_fixed_width() {
        let ts = now();
        assert_eq!(ts.len(), "2026-10-07T14:02:11.482Z".len(), "{ts}");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn to_minutes_truncates() {
        assert_eq!(to_minutes("2026-10-07T14:02:11.482Z"), "2026-10-07T14:02Z");
    }

    #[test]
    fn to_local_formats_or_passes_through() {
        assert_eq!(
            to_local("2026-10-07T14:02:11.482Z").len(),
            "2026-10-07 14:02".len()
        );
        assert_eq!(to_local("garbage"), "garbage");
    }
}
