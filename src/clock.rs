use jiff::Timestamp;

/// Injectable clock so quota windows can be tested with a frozen time.
#[derive(Debug, Clone)]
pub struct Clock {
    frozen: Option<Timestamp>,
}

impl Clock {
    pub fn system() -> Self {
        Self { frozen: None }
    }

    pub fn frozen(now: Timestamp) -> Self {
        Self { frozen: Some(now) }
    }

    pub fn now(&self) -> Timestamp {
        self.frozen.unwrap_or_else(Timestamp::now)
    }

    pub fn rfc3339(&self) -> String {
        crate::rfc3339::millis(self.now())
    }
}

/// UTC instant for unit tests. Unavailable to integration tests.
#[cfg(test)]
pub fn utc(year: i16, month: i8, day: i8, hour: i8, minute: i8, second: i8) -> Timestamp {
    jiff::civil::date(year, month, day)
        .at(hour, minute, second, 0)
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .expect("valid test time")
        .timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_clock_does_not_advance() {
        let t = utc(2026, 10, 13, 12, 0, 0);
        let c = Clock::frozen(t);
        assert_eq!(c.now(), t);
        assert_eq!(c.now(), t);
    }
}
