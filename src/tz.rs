use anyhow::{bail, Result};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime};

/// Display zone for status, usage, tasks, and reports.
/// Empty / `local` uses the system local zone.
#[derive(Debug, Clone)]
pub struct DisplayTz {
    pub label: String,
    named: Option<TimeZone>,
}

fn lookup(name: &str) -> Option<TimeZone> {
    jiff::tz::TimeZoneDatabase::bundled().get(name).ok()
}

impl DisplayTz {
    pub fn system() -> Self {
        if let Some(from_env) = std::env::var("TZ")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && *s != "local")
        {
            if let Some(tz) = lookup(&from_env) {
                return Self {
                    label: from_env,
                    named: Some(tz),
                };
            }
        }
        // LaunchAgents often have no TZ. Prefer the OS zone so "local"
        // is the machine's wall clock, not UTC.
        if let Some(iana) = iana_from_localtime() {
            if let Some(tz) = lookup(&iana) {
                let label = tz.iana_name().unwrap_or(&iana).to_string();
                return Self {
                    label,
                    named: Some(tz),
                };
            }
        }
        Self {
            label: "local".into(),
            named: None,
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("local") {
            return Ok(Self::system());
        }
        match lookup(s) {
            Some(tz) => {
                let label = tz.iana_name().unwrap_or(s).to_string();
                Ok(Self {
                    label,
                    named: Some(tz),
                })
            }
            None => bail!("timezone {s:?} is not a valid IANA zone (or local)"),
        }
    }

    fn zone(&self) -> TimeZone {
        match &self.named {
            Some(tz) => tz.clone(),
            None => TimeZone::system(),
        }
    }

    fn zoned(&self, utc: Timestamp) -> ZonedDateTime {
        utc.to_zoned_datetime(self.zone())
    }

    pub fn day(&self, utc: Timestamp) -> String {
        format!("{}", self.zoned(utc).strftime("%Y-%m-%d"))
    }

    pub fn format(&self, utc: Timestamp) -> String {
        format!(
            "{} {}",
            self.zoned(utc).strftime("%Y-%m-%d %H:%M"),
            self.label
        )
    }

    /// Compact dashboard stamp without the zone name (the page header shows it
    /// once): always `Mon DD` (date only), e.g. `Oct 16`; the full stamp goes in a hover title.
    pub fn format_compact(&self, utc: Timestamp) -> String {
        format!("{}", self.zoned(utc).strftime("%b %d"))
    }

    /// Short stamp for usage lines, e.g. `Oct 16 07:36 UTC`.
    /// The day is unpadded (`Oct 3`, not `Oct 03`).
    pub fn format_short(&self, utc: Timestamp) -> String {
        let t = self.zoned(utc);
        format!(
            "{} {} {:02}:{:02} {}",
            t.strftime("%b"),
            t.day(),
            t.hour(),
            t.minute(),
            self.label
        )
    }

    pub fn format_opt(&self, rfc3339: Option<&str>) -> String {
        rfc3339
            .and_then(crate::rfc3339::parse)
            .map(|t| self.format(t))
            .unwrap_or_else(|| "-".into())
    }

    pub fn format_str(&self, rfc3339: &str) -> String {
        self.format_opt(Some(rfc3339))
    }

    pub fn parse_utc(rfc3339: &str) -> Option<Timestamp> {
        crate::rfc3339::parse(rfc3339)
    }

    /// Wall-clock hour/minute in this zone.
    pub fn hm(&self, utc: Timestamp) -> (u32, u32) {
        let t = self.zoned(utc);
        (t.hour() as u32, t.minute() as u32)
    }

    /// Grok-style `11:04 AM` in this zone (no leading zero on the hour).
    pub fn format_ampm(&self, utc: Timestamp) -> String {
        let (h, m) = self.hm(utc);
        ampm_label(h, m)
    }

    pub fn now_local_hm(&self) -> (u32, u32) {
        self.hm(Timestamp::now())
    }

    pub fn today(&self) -> String {
        self.day(Timestamp::now())
    }
}

pub(crate) fn ampm_label(hour24: u32, minute: u32) -> String {
    let (h12, suffix) = match hour24 {
        0 => (12, "AM"),
        1..=11 => (hour24, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    format!("{h12}:{minute:02} {suffix}")
}

/// IANA name from `/etc/localtime` (macOS: `…/zoneinfo/America/Los_Angeles`).
fn iana_from_localtime() -> Option<String> {
    let dest = std::fs::read_link("/etc/localtime").ok()?;
    let s = dest.to_string_lossy();
    for marker in ["zoneinfo/", "timezone/"] {
        if let Some(idx) = s.rfind(marker) {
            let rest = s[idx + marker.len()..].trim_matches('/').trim();
            if !rest.is_empty() && lookup(rest).is_some() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_and_label() {
        let tz = DisplayTz::parse("UTC").unwrap();
        let t = crate::clock::utc(2026, 10, 13, 23, 30, 0);
        assert_eq!(tz.day(t), "2026-10-13");
        assert!(tz.format(t).contains("2026-10-13 23:30"));
        assert!(tz.format(t).contains("UTC"));
        assert_eq!(tz.format_short(t), "Oct 13 23:30 UTC");
        assert_eq!(tz.format_compact(t), "Oct 13");
        let far = crate::clock::utc(2026, 11, 3, 3, 0, 0);
        assert_eq!(tz.format_compact(far), "Nov 03");
        let vil = DisplayTz::parse("Europe/Vilnius").unwrap();
        assert!(!vil.format_compact(t).contains("Vilnius"));
    }

    #[test]
    fn new_york_rolls_the_day() {
        let tz = DisplayTz::parse("America/New_York").unwrap();
        let t = crate::clock::utc(2026, 10, 14, 2, 0, 0);
        assert_eq!(tz.day(t), "2026-10-13");
    }

    #[test]
    fn short_reset_is_local_wall_clock_with_zone() {
        let tz = DisplayTz::parse("America/New_York").unwrap();
        let t = crate::clock::utc(2026, 10, 14, 17, 10, 0);
        let s = tz.format_short(t);
        assert!(s.contains("America/New_York"), "{s}");
        assert!(
            s.contains("13:10"),
            "17:10 UTC must become 13:10 EDT, got {s}"
        );
        assert!(
            !s.contains("17:10"),
            "must not print the UTC clock, got {s}"
        );
        assert_eq!(tz.format_ampm(t), "1:10 PM");
    }

    #[test]
    fn ampm_label_matches_grok_clock() {
        assert_eq!(ampm_label(0, 4), "12:04 AM");
        assert_eq!(ampm_label(11, 4), "11:04 AM");
        assert_eq!(ampm_label(12, 0), "12:00 PM");
        assert_eq!(ampm_label(23, 4), "11:04 PM");
        let utc = DisplayTz::parse("UTC").unwrap();
        let t = crate::clock::utc(2026, 10, 28, 11, 4, 0);
        assert_eq!(utc.format_ampm(t), "11:04 AM");
    }
}

#[cfg(test)]
mod pins {
    use super::*;

    #[test]
    fn utc_stamp() {
        let tz = DisplayTz::parse("UTC").unwrap();
        let t = DisplayTz::parse_utc("2026-10-13T23:30:00Z").unwrap();
        assert_eq!(tz.day(t), "2026-10-13");
        let formatted = tz.format(t);
        assert!(formatted.contains("2026-10-13 23:30"), "{formatted}");
        assert!(formatted.contains("UTC"), "{formatted}");
        assert_eq!(tz.format_short(t), "Oct 13 23:30 UTC");
        assert_eq!(tz.format_compact(t), "Oct 13");
        assert_eq!(tz.hm(t), (23, 30));
    }

    #[test]
    fn november_compact_pads_the_day() {
        let tz = DisplayTz::parse("UTC").unwrap();
        let t = DisplayTz::parse_utc("2026-11-03T03:00:00Z").unwrap();
        assert_eq!(tz.format_compact(t), "Nov 03");
    }

    #[test]
    fn new_york_rolls_the_day_and_prints_wall_clock() {
        let tz = DisplayTz::parse("America/New_York").unwrap();
        let rolled = DisplayTz::parse_utc("2026-10-14T02:00:00Z").unwrap();
        assert_eq!(tz.day(rolled), "2026-10-13");
        let t = DisplayTz::parse_utc("2026-10-14T17:10:00Z").unwrap();
        let short = tz.format_short(t);
        assert!(short.contains("America/New_York"), "{short}");
        assert!(short.contains("13:10"), "{short}");
        assert!(!short.contains("17:10"), "{short}");
        assert_eq!(tz.format_ampm(t), "1:10 PM");
        assert_eq!(tz.hm(t), (13, 10));
    }

    #[test]
    fn dubai_is_four_hours_ahead() {
        let tz = DisplayTz::parse("Asia/Dubai").unwrap();
        let t = DisplayTz::parse_utc("2026-10-13T12:00:00Z").unwrap();
        let formatted = tz.format(t);
        assert!(formatted.contains("16:00"), "{formatted}");
        assert!(formatted.contains("Asia/Dubai"), "{formatted}");
    }

    #[test]
    fn kolkata_and_lord_howe() {
        let t = DisplayTz::parse_utc("2026-10-13T12:00:00Z").unwrap();
        let kolkata = DisplayTz::parse("Asia/Kolkata").unwrap();
        assert_eq!(kolkata.day(t), "2026-10-13");
        assert_eq!(kolkata.format_short(t), "Oct 13 17:30 Asia/Kolkata");
        let howe = DisplayTz::parse("Australia/Lord_Howe").unwrap();
        assert_eq!(howe.day(t), "2026-10-13");
        assert_eq!(howe.format_short(t), "Oct 13 23:00 Australia/Lord_Howe");
    }

    #[test]
    fn legacy_and_alias_labels_parse() {
        for label in [
            "UTC",
            "Etc/UTC",
            "GMT",
            "US/Eastern",
            "Europe/Kiev",
            "Etc/GMT+5",
        ] {
            assert!(DisplayTz::parse(label).is_ok(), "{label}");
        }
    }

    #[test]
    fn unknown_zone_error_text() {
        let err = DisplayTz::parse("Not/AZone").unwrap_err();
        assert_eq!(
            err.to_string(),
            "timezone \"Not/AZone\" is not a valid IANA zone (or local)"
        );
    }

    #[test]
    fn empty_and_local_use_the_host_zone() {
        assert!(DisplayTz::parse("").is_ok());
        assert!(DisplayTz::parse("local").is_ok());
    }
}
