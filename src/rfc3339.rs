//! RFC 3339 strings written to the database, JSON, and logs.
//!
//! `millis` is the canonical stored form (`…sssZ`, always three fractional
//! digits, truncated). `secs` is the whole-second `Z` form used for day-range
//! SQL bounds. `auto` is the numeric-offset form (`+00:00`, fractional digits
//! in groups of three). Do not format an instant with `Display`.

use jiff::fmt::temporal::{DateTimePrinter, Pieces};
use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, ZonedDateTime};

const MILLIS: DateTimePrinter = DateTimePrinter::new().precision(Some(3));
const SECS: DateTimePrinter = DateTimePrinter::new().precision(Some(0));

pub fn millis(t: Timestamp) -> String {
    MILLIS.timestamp_to_string(&t)
}

pub fn secs(t: Timestamp) -> String {
    SECS.timestamp_to_string(&t)
}

pub fn auto(t: Timestamp) -> String {
    let ns = t.subsec_nanosecond().unsigned_abs();
    let p = if ns == 0 {
        0
    } else if ns.is_multiple_of(1_000_000) {
        3
    } else if ns.is_multiple_of(1_000) {
        6
    } else {
        9
    };
    DateTimePrinter::new()
        .precision(Some(p))
        .timestamp_with_offset_to_string(&t, Offset::UTC)
}

pub fn parse(s: &str) -> Option<Timestamp> {
    s.parse().ok()
}

/// Keeps the string's own offset (`ZonedDateTime` in that fixed zone).
pub fn parse_fixed(s: &str) -> Option<ZonedDateTime> {
    let t: Timestamp = s.parse().ok()?;
    let off = Pieces::parse(s).ok()?.to_numeric_offset()?;
    Some(t.to_zoned_datetime(TimeZone::fixed(off)))
}

#[cfg(test)]
mod pins {
    use super::*;

    #[test]
    fn millis_pads_whole_seconds() {
        let t = parse("2026-10-13T12:00:00Z").unwrap();
        assert_eq!(millis(t), "2026-10-13T12:00:00.000Z");
    }

    #[test]
    fn millis_truncates_sub_millis_not_rounds() {
        let t = parse("2026-10-13T12:00:00.120456789+00:00").unwrap();
        assert_eq!(millis(t), "2026-10-13T12:00:00.120Z");
    }

    #[test]
    fn millis_truncates_just_below_the_next_second() {
        let t = parse("2026-10-13T12:00:00.999999999Z").unwrap();
        assert_eq!(millis(t), "2026-10-13T12:00:00.999Z");
    }

    #[test]
    fn millis_converts_a_positive_offset_to_utc() {
        let t = parse("2026-10-13T00:00:00+05:30").unwrap();
        assert_eq!(millis(t), "2026-10-12T18:30:00.000Z");
    }

    #[test]
    fn millis_pads_a_pre_epoch_half_second() {
        let t = parse("1969-12-31T23:59:59.5Z").unwrap();
        assert_eq!(millis(t), "1969-12-31T23:59:59.500Z");
    }

    #[test]
    fn secs_drops_the_fraction() {
        let t = parse("2026-10-13T12:00:00.999Z").unwrap();
        assert_eq!(secs(t), "2026-10-13T12:00:00Z");
    }

    #[test]
    fn auto_whole_second_uses_numeric_offset() {
        let t = parse("2026-10-13T12:00:00Z").unwrap();
        assert_eq!(auto(t), "2026-10-13T12:00:00+00:00");
    }

    #[test]
    fn auto_exact_millis_keeps_three_digits() {
        let t = parse("2026-10-13T12:00:00.120Z").unwrap();
        assert_eq!(auto(t), "2026-10-13T12:00:00.120+00:00");
    }

    #[test]
    fn auto_nanos_keep_nine_digits() {
        let t = parse("2026-10-13T12:00:00.123456789Z").unwrap();
        assert_eq!(auto(t), "2026-10-13T12:00:00.123456789+00:00");
    }

    #[test]
    fn auto_micros_keep_six_digits() {
        let t = parse("2026-10-13T12:00:00.000500Z").unwrap();
        assert_eq!(auto(t), "2026-10-13T12:00:00.000500+00:00");
    }

    #[test]
    fn parse_accepts_rfc3339_variants_and_round_trips_to_utc_millis() {
        let cases = [
            ("2026-10-13T12:00:00Z", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T12:00:00z", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13t12:00:00Z", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13 12:00:00Z", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T12:00:00+00:00", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T12:00:00-00:00", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T17:30:00+05:30", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T07:00:00-05:00", "2026-10-13T12:00:00.000Z"),
            ("2026-10-13T12:00:00.1Z", "2026-10-13T12:00:00.100Z"),
            ("2026-10-13T12:00:00.12Z", "2026-10-13T12:00:00.120Z"),
            ("2026-10-13T12:00:00.123Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.1234Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.12345Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.123456Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.1234567Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.12345678Z", "2026-10-13T12:00:00.123Z"),
            ("2026-10-13T12:00:00.123456789Z", "2026-10-13T12:00:00.123Z"),
        ];
        for (input, expected) in cases {
            let t = parse(input).unwrap_or_else(|| panic!("parse {input}"));
            assert_eq!(millis(t), expected, "{input}");
        }
    }

    #[test]
    fn parse_rejects_non_rfc3339() {
        for input in [
            "2026-10-13T12:00:00",
            "2026-10-13",
            "2027-01-12 07:28:00 UTC",
            "2026-02-30T12:00:00Z",
            " 2026-10-13T12:00:00Z",
            "ZZ",
            "1791968400",
            "",
        ] {
            assert!(parse(input).is_none(), "accepted {input:?}");
        }
    }

    #[test]
    fn millis_strings_sort_in_time_order_and_historical_mix_parses() {
        let plus = "2026-10-13T12:00:00.000+00:00";
        let dotted_z = "2026-10-13T12:00:00.000Z";
        let secs_z = "2026-10-13T12:00:00Z";
        assert!(plus < dotted_z, "{plus} < {dotted_z}");
        assert!(
            dotted_z < secs_z,
            "secs Z form sorts after millis of the same second"
        );
        let via_plus = millis(parse(plus).unwrap());
        let via_dotted = millis(parse(dotted_z).unwrap());
        let via_secs = millis(parse(secs_z).unwrap());
        assert_eq!(via_plus, "2026-10-13T12:00:00.000Z");
        assert_eq!(via_plus, via_dotted);
        assert_eq!(via_dotted, via_secs);
    }
}
