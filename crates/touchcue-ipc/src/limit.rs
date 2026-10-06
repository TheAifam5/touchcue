//! Rate limit for repeated warnings.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Shortest interval between two log events of one kind.
const INTERVAL: Duration = Duration::from_secs(1);

/// Allows one log event per [`INTERVAL`] and counts the suppressed ones.
#[derive(Debug)]
pub(crate) struct LogLimit {
    base: Instant,
    /// Milliseconds after `base` from which the next event is allowed.
    next_ms: AtomicU64,
    suppressed: AtomicU64,
}

impl LogLimit {
    pub(crate) fn new() -> LogLimit {
        LogLimit {
            base: Instant::now(),
            next_ms: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Returns the number of events suppressed since the last allowed one if
    /// this event may be logged now, or `None` after counting it as suppressed.
    pub(crate) fn allow(&self) -> Option<u64> {
        self.allow_at(Instant::now())
    }

    fn allow_at(&self, now: Instant) -> Option<u64> {
        let now_ms = millis(now.saturating_duration_since(self.base));
        let next = self.next_ms.load(Ordering::Acquire);
        let due = now_ms >= next
            && match self.next_ms.compare_exchange(
                next,
                now_ms.saturating_add(millis(INTERVAL)),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_claimed) => true,
                // Another caller claimed this interval first.
                Err(_current) => false,
            };
        if due {
            Some(self.suppressed.swap(0, Ordering::AcqRel))
        } else {
            self.suppressed.fetch_add(1, Ordering::AcqRel);
            None
        }
    }
}

fn millis(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(duration.subsec_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_one_event_per_interval_and_counts_the_rest() {
        let limit = LogLimit::new();
        let at = |ms| limit.base + Duration::from_millis(ms);
        assert_eq!(limit.allow_at(at(0)), Some(0));
        assert_eq!(limit.allow_at(at(10)), None);
        assert_eq!(limit.allow_at(at(999)), None);
        assert_eq!(limit.allow_at(at(1000)), Some(2));
        assert_eq!(limit.allow_at(at(1500)), None);
        assert_eq!(limit.allow_at(at(5000)), Some(1));
    }
}
