use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// Display zone for status, usage, tasks, and reports.
/// Empty / `local` uses the system local zone.
#[derive(Debug, Clone)]
pub struct DisplayTz {
    pub label: String,
    named: Option<Tz>,
}

impl DisplayTz {
    pub fn system() -> Self {
        if let Some(from_env) = std::env::var("TZ")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && *s != "local")
        {
            if let Ok(tz) = from_env.parse::<Tz>() {
                return Self {
                    label: from_env,
                    named: Some(tz),
                };
            }
        }
        // LaunchAgents often have no TZ. Prefer the OS zone so "local"
        // is the machine's wall clock, not UTC.
        if let Some(iana) = iana_from_localtime() {
            if let Ok(tz) = iana.parse::<Tz>() {
                return Self {
                    label: tz.name().to_string(),
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
        match s.parse::<Tz>() {
            Ok(tz) => Ok(Self {
                label: tz.name().to_string(),
                named: Some(tz),
            }),
            Err(_) => bail!("display_timezone {s:?} is not a valid IANA zone (or local)"),
        }
    }

    pub fn day(&self, utc: DateTime<Utc>) -> String {
        match self.named {
            Some(tz) => utc.with_timezone(&tz).format("%Y-%m-%d").to_string(),
            None => utc.with_timezone(&Local).format("%Y-%m-%d").to_string(),
        }
    }

    pub fn format(&self, utc: DateTime<Utc>) -> String {
        match self.named {
            Some(tz) => format!(
                "{} {}",
                utc.with_timezone(&tz).format("%Y-%m-%d %H:%M"),
                self.label
            ),
            None => format!(
                "{} {}",
                utc.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                self.label
            ),
        }
    }

    /// Compact dashboard stamp without the zone name (the page header shows it
    /// once): always `Mon DD` (date only), e.g. `Oct 16`; the full stamp goes in a hover title.
    pub fn format_compact(&self, utc: DateTime<Utc>) -> String {
        match self.named {
            Some(tz) => utc.with_timezone(&tz).format("%b %d").to_string(),
            None => utc.with_timezone(&Local).format("%b %d").to_string(),
        }
    }

    /// Short stamp for usage lines, e.g. `Oct 16 07:36 UTC`.
    pub fn format_short(&self, utc: DateTime<Utc>) -> String {
        let (month, day, hour, minute) = match self.named {
            Some(tz) => {
                let t = utc.with_timezone(&tz);
                (t.format("%b").to_string(), t.day(), t.hour(), t.minute())
            }
            None => {
                let t = utc.with_timezone(&Local);
                (t.format("%b").to_string(), t.day(), t.hour(), t.minute())
            }
        };
        format!("{month} {day} {hour:02}:{minute:02} {}", self.label)
    }

    pub fn format_opt(&self, rfc3339: Option<&str>) -> String {
        rfc3339
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| self.format(t.with_timezone(&Utc)))
            .unwrap_or_else(|| "-".into())
    }

    pub fn format_str(&self, rfc3339: &str) -> String {
        self.format_opt(Some(rfc3339))
    }

    pub fn parse_utc(rfc3339: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(rfc3339)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// Wall-clock hour/minute in this zone.
    pub fn hm(&self, utc: DateTime<Utc>) -> (u32, u32) {
        match self.named {
            Some(tz) => {
                let t = utc.with_timezone(&tz);
                (t.hour(), t.minute())
            }
            None => {
                let t = utc.with_timezone(&Local);
                (t.hour(), t.minute())
            }
        }
    }

    /// Grok-style `11:04 AM` in this zone (no leading zero on the hour).
    pub fn format_ampm(&self, utc: DateTime<Utc>) -> String {
        let (h, m) = self.hm(utc);
        ampm_label(h, m)
    }

    pub fn now_local_hm(&self) -> (u32, u32) {
        match self.named {
            Some(tz) => {
                let t = tz.from_utc_datetime(&Utc::now().naive_utc());
                (t.hour(), t.minute())
            }
            None => {
                let t = Local::now();
                (t.hour(), t.minute())
            }
        }
    }

    pub fn today(&self) -> String {
        self.day(Utc::now())
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
            if !rest.is_empty() && rest.parse::<Tz>().is_ok() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn utc_day_and_label() {
        let tz = DisplayTz::parse("UTC").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 23, 30, 0).unwrap();
        assert_eq!(tz.day(t), "2026-10-13");
        assert!(tz.format(t).contains("2026-10-13 23:30"));
        assert!(tz.format(t).contains("UTC"));
        assert_eq!(tz.format_short(t), "Oct 13 23:30 UTC");
        assert_eq!(tz.format_compact(t), "Oct 13");
        let far = Utc.with_ymd_and_hms(2026, 11, 3, 3, 0, 0).unwrap();
        assert_eq!(tz.format_compact(far), "Nov 03");
        let vil = DisplayTz::parse("Europe/Vilnius").unwrap();
        assert!(!vil.format_compact(t).contains("Vilnius"));
    }

    #[test]
    fn new_york_rolls_the_day() {
        let tz = DisplayTz::parse("America/New_York").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 10, 14, 2, 0, 0).unwrap();
        assert_eq!(tz.day(t), "2026-10-13");
    }

    #[test]
    fn short_reset_is_local_wall_clock_with_zone() {
        let tz = DisplayTz::parse("America/New_York").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 10, 14, 17, 10, 0).unwrap();
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
        let t = Utc.with_ymd_and_hms(2026, 10, 28, 11, 4, 0).unwrap();
        assert_eq!(utc.format_ampm(t), "11:04 AM");
    }
}
