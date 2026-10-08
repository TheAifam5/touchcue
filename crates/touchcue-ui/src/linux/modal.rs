//! When a request may block clicks with a modal overlay.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use touchcue_core::RequestId;

/// Longest time an overlay stays on an output, however many requests hold it.
pub(crate) const MODAL_MAX: Duration = Duration::from_secs(120);
/// Time after an overlay reached [`MODAL_MAX`] during which its output
/// gets no new overlay.
pub(crate) const MODAL_COOLDOWN: Duration = Duration::from_secs(30);

/// Which shown requests may hold modal overlays.
#[derive(Debug, Default)]
pub(crate) struct ModalTimer {
    /// `true` while the request may hold overlays.
    entries: BTreeMap<RequestId, bool>,
}

impl ModalTimer {
    /// Records that `id` is shown, waiting for a touch or not, and reports
    /// whether it may hold overlays.
    ///
    /// A request may hold overlays from the first time it is shown waiting
    /// until it stops waiting or is dismissed, and never again after that.
    pub(crate) fn sync(&mut self, id: RequestId, waiting: bool) -> bool {
        let active = self.entries.entry(id).or_insert(waiting);
        *active &= waiting;
        *active
    }

    /// Takes the overlays from `id` for good.
    pub(crate) fn dismiss(&mut self, id: RequestId) {
        if let Some(active) = self.entries.get_mut(&id) {
            *active = false;
        }
    }

    /// Forgets a hidden request.
    pub(crate) fn remove(&mut self, id: RequestId) {
        self.entries.remove(&id);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// An overlay is shown until `until`.
    Shown { until: Instant },
    /// No overlay may be shown before `until`.
    Cooling { until: Instant },
}

/// Overlay lifetimes per output, identified by `K`.
///
/// Every method takes the current time, so the schedule is deterministic.
#[derive(Debug)]
pub(crate) struct OverlayClock<K> {
    slots: Vec<(K, Slot)>,
}

impl<K> Default for OverlayClock<K> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<K: Clone + PartialEq> OverlayClock<K> {
    /// Reports whether an overlay may be created on `key` at `now`, and
    /// records it as shown until [`MODAL_MAX`] from now when it may.
    ///
    /// Requests joining an overlay that already exists do not call this, so
    /// they never extend its lifetime.
    pub(crate) fn start(&mut self, key: &K, now: Instant) -> bool {
        let until = now.checked_add(MODAL_MAX).unwrap_or(now);
        match self.slots.iter_mut().find(|(other, _)| other == key) {
            Some((_, Slot::Cooling { until: cooled })) if now < *cooled => false,
            Some((_, slot)) => {
                *slot = Slot::Shown { until };
                true
            }
            None => {
                self.slots.push((key.clone(), Slot::Shown { until }));
                true
            }
        }
    }

    /// Records that the overlay on `key` was removed because no request
    /// holds it any more or a click dismissed it; no cooldown follows.
    pub(crate) fn ended(&mut self, key: &K) {
        self.slots
            .retain(|(other, slot)| other != key || matches!(slot, Slot::Cooling { .. }));
    }

    /// Moves the overlay shown on `from` to `to`, keeping its time limit,
    /// and reports whether it may stay; it may not while `to` cools down at
    /// `now`, and `from` is then ended.
    ///
    /// The caller has no overlay on `to`. Without a shown overlay on `from`,
    /// `to` gets a full [`MODAL_MAX`] from `now`.
    pub(crate) fn rekey(&mut self, from: &K, to: K, now: Instant) -> bool {
        let until = self.slots.iter().find_map(|(key, slot)| match slot {
            Slot::Shown { until } if key == from => Some(*until),
            Slot::Shown { .. } | Slot::Cooling { .. } => None,
        });
        self.ended(from);
        let cooling = self.slots.iter().any(|(key, slot)| {
            *key == to && matches!(slot, Slot::Cooling { until } if now < *until)
        });
        if cooling {
            return false;
        }
        let until = until.unwrap_or_else(|| now.checked_add(MODAL_MAX).unwrap_or(now));
        self.slots.retain(|(key, _)| *key != to);
        self.slots.push((to, Slot::Shown { until }));
        true
    }

    /// Returns the outputs whose overlay reached [`MODAL_MAX`] at `now`,
    /// which then cool down, and forgets finished cooldowns.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<K> {
        let cooled = now.checked_add(MODAL_COOLDOWN).unwrap_or(now);
        let mut expired = Vec::new();
        for (key, slot) in &mut self.slots {
            if let Slot::Shown { until } = *slot
                && until <= now
            {
                *slot = Slot::Cooling { until: cooled };
                expired.push(key.clone());
            }
        }
        self.slots
            .retain(|(_, slot)| !matches!(slot, Slot::Cooling { until } if *until <= now));
        expired
    }

    /// Returns the earliest time [`OverlayClock::expire`] removes an overlay.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter_map(|(_, slot)| match slot {
                Slot::Shown { until } => Some(*until),
                Slot::Cooling { .. } => None,
            })
            .min()
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }
}

/// Returns the 8-bit alpha of black that dims by `dim`, from 0.0 to 1.0.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is rounded and clamped to 0..=255 first"
)]
pub(crate) fn dim_alpha(dim: f32) -> u8 {
    (dim * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: RequestId = RequestId(1);
    const B: RequestId = RequestId(2);

    fn later(start: Instant, s: u64) -> Instant {
        start + Duration::from_secs(s)
    }

    #[test]
    fn waiting_request_may_hold_overlays_until_it_stops_waiting() {
        let mut timer = ModalTimer::default();
        assert!(timer.sync(A, true));
        assert!(timer.sync(A, true));
        assert!(!timer.sync(A, false));
        // A retry that waits again does not bring the overlay back.
        assert!(!timer.sync(A, true));
    }

    #[test]
    fn request_shown_after_it_ended_never_holds_one() {
        let mut timer = ModalTimer::default();
        assert!(!timer.sync(A, false));
        assert!(!timer.sync(A, true));
    }

    #[test]
    fn dismissed_request_never_holds_one_again() {
        let mut timer = ModalTimer::default();
        assert!(timer.sync(A, true));
        assert!(timer.sync(B, true));
        timer.dismiss(A);
        assert!(!timer.sync(A, true));
        assert!(timer.sync(B, true));
        timer.dismiss(RequestId(9));
        assert!(!timer.sync(RequestId(9), false));
        timer.remove(B);
        assert!(!timer.sync(B, false));
        timer.clear();
        assert!(timer.sync(A, true));
    }

    #[test]
    fn rekeyed_overlay_keeps_its_limit_and_respects_cooldown() {
        let t0 = Instant::now();
        let mut clock = OverlayClock::default();
        assert!(clock.start(&None, t0));
        assert!(clock.rekey(&None, Some("DP-1"), later(t0, 10)));
        assert_eq!(clock.next_deadline(), Some(later(t0, 120)));
        assert_eq!(clock.expire(later(t0, 120)), [Some("DP-1")]);
        // DP-1 cools down: an overlay that lands there may not stay.
        assert!(clock.start(&None, later(t0, 130)));
        assert!(!clock.rekey(&None, Some("DP-1"), later(t0, 131)));
        assert_eq!(clock.next_deadline(), None);
        assert!(clock.start(&None, later(t0, 160)));
        assert!(clock.rekey(&None, Some("DP-1"), later(t0, 161)));
    }

    /// A creates the overlay at 0 s; B at 100 s and C at 200 s would only
    /// join or recreate it.
    #[test]
    fn chained_requests_cannot_extend_an_overlay() {
        let t0 = Instant::now();
        let mut clock = OverlayClock::default();
        assert!(clock.start(&"DP-1", t0));
        // B joins the existing overlay at 100 s without calling `start`.
        assert_eq!(clock.next_deadline(), Some(later(t0, 120)));
        assert_eq!(clock.expire(later(t0, 119)), Vec::<&str>::new());
        assert_eq!(clock.expire(later(t0, 120)), ["DP-1"]);
        assert_eq!(clock.next_deadline(), None);
        // Within the cooldown no overlay is created on that output.
        assert!(!clock.start(&"DP-1", later(t0, 130)));
        assert!(!clock.start(&"DP-1", later(t0, 149)));
        assert!(clock.start(&"DP-2", later(t0, 130)));
        // C at 200 s, after the cooldown, gets a fresh overlay.
        assert!(clock.start(&"DP-1", later(t0, 200)));
        assert_eq!(clock.expire(later(t0, 319)), ["DP-2"]);
        assert_eq!(clock.next_deadline(), Some(later(t0, 320)));
    }

    #[test]
    fn overlay_without_holders_ends_without_cooldown() {
        let t0 = Instant::now();
        let mut clock = OverlayClock::default();
        assert!(clock.start(&1, t0));
        // The last holder let go, or a click dismissed the overlay.
        clock.ended(&1);
        assert_eq!(clock.next_deadline(), None);
        assert!(clock.start(&1, later(t0, 1)));
        assert_eq!(clock.next_deadline(), Some(later(t0, 121)));
    }

    #[test]
    fn ending_does_not_cut_a_cooldown_short() {
        let t0 = Instant::now();
        let mut clock = OverlayClock::default();
        assert!(clock.start(&1, t0));
        assert_eq!(clock.expire(later(t0, 120)), [1]);
        clock.ended(&1);
        assert!(!clock.start(&1, later(t0, 121)));
        clock.clear();
        assert!(clock.start(&1, later(t0, 121)));
    }

    #[test]
    fn finished_cooldowns_are_forgotten() {
        let t0 = Instant::now();
        let mut clock = OverlayClock::default();
        assert!(clock.start(&1, t0));
        assert_eq!(clock.expire(later(t0, 120)), [1]);
        assert_eq!(clock.expire(later(t0, 150)), Vec::<i32>::new());
        assert_eq!(clock.slots.len(), 0);
    }

    #[test]
    fn dim_maps_to_alpha() {
        assert_eq!(dim_alpha(0.0), 0);
        assert_eq!(dim_alpha(0.4), 102);
        assert_eq!(dim_alpha(1.0), 255);
        assert_eq!(dim_alpha(2.0), 255);
        assert_eq!(dim_alpha(-1.0), 0);
    }
}
