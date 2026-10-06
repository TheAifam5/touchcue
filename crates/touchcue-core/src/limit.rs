//! Rate limiting of repeating log events.

use std::time::{Duration, Instant};

/// Lets one event through per interval and counts the ones held back.
#[derive(Debug)]
pub struct RateLimit {
    interval: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl RateLimit {
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            suppressed: 0,
        }
    }

    /// Returns how many events were held back since the last one let
    /// through, or `None` when the event at `now` is held back.
    pub fn check(&mut self, now: Instant) -> Option<u64> {
        let due = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval);
        if due {
            self.last = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }

    /// Calls `log` with the number of events held back when the event at
    /// `now` is let through.
    pub fn log(&mut self, now: Instant, log: impl FnOnce(u64)) {
        if let Some(suppressed) = self.check(now) {
            log(suppressed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lets_one_event_through_per_interval() {
        let t0 = Instant::now();
        let mut limit = RateLimit::new(Duration::from_secs(60));
        assert_eq!(limit.check(t0), Some(0));
        assert_eq!(limit.check(t0 + Duration::from_secs(1)), None);
        assert_eq!(limit.check(t0 + Duration::from_secs(59)), None);
        assert_eq!(limit.check(t0 + Duration::from_secs(60)), Some(2));
        assert_eq!(limit.check(t0 + Duration::from_secs(61)), None);
    }
}
