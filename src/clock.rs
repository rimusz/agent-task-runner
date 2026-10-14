use chrono::{DateTime, Utc};

/// Injectable clock so quota windows can be tested with a frozen time.
#[derive(Debug, Clone)]
pub struct Clock {
    frozen: Option<DateTime<Utc>>,
}

impl Clock {
    pub fn system() -> Self {
        Self { frozen: None }
    }

    pub fn frozen(now: DateTime<Utc>) -> Self {
        Self { frozen: Some(now) }
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.frozen.unwrap_or_else(Utc::now)
    }

    pub fn rfc3339(&self) -> String {
        self.now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn frozen_clock_does_not_advance() {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let c = Clock::frozen(t);
        assert_eq!(c.now(), t);
        assert_eq!(c.now(), t);
    }
}
