//! State marks drawn over the popup icon and the clock that animates them.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use touchcue_core::{EndReason, RequestId, RequestState};

/// Length of the transition from one mark to the next.
pub(crate) const TRANSITION: Duration = Duration::from_millis(250);
/// Time between two animation frames, about 30 per second.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Time the spinner takes for one turn.
const TURN: Duration = Duration::from_millis(1200);

/// What the overlay shows for a request state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The request waits for a touch.
    Spinner,
    /// The authenticator was touched.
    Check,
    /// The operation was cancelled or failed.
    Cross,
    /// The authenticator stopped waiting for a touch.
    Clock,
}

impl Mark {
    pub(crate) fn of(state: RequestState) -> Self {
        match state {
            RequestState::Waiting => Self::Spinner,
            RequestState::Lingering(EndReason::Touched) => Self::Check,
            RequestState::Lingering(EndReason::Cancelled | EndReason::Failed) => Self::Cross,
            RequestState::Lingering(EndReason::TimedOut) => Self::Clock,
        }
    }
}

/// One frame of the overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Frame {
    pub(crate) mark: Mark,
    /// Mark fading out while `mark` fades in; `None` outside a transition.
    pub(crate) previous: Option<Mark>,
    /// Eased progress of the transition to `mark`, from 0.0 to 1.0.
    pub(crate) progress: f32,
    /// Spinner rotation in turns, from 0.0 up to 1.0.
    pub(crate) spin: f32,
}

impl Frame {
    /// Returns the still frame of `mark`: no transition, spinner at rest.
    pub(crate) fn settled(mark: Mark) -> Self {
        Self {
            mark,
            previous: None,
            progress: 1.0,
            spin: 0.0,
        }
    }
}

/// Returns the eased progress of a transition `elapsed` after it started:
/// 0.0 at the start, 1.0 from [`TRANSITION`] on, decelerating in between.
pub(crate) fn progress(elapsed: Duration) -> f32 {
    let linear = (elapsed.as_secs_f32() / TRANSITION.as_secs_f32()).clamp(0.0, 1.0);
    1.0 - (1.0 - linear).powi(3)
}

/// Returns the spinner rotation in turns `elapsed` after it started.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a fraction of a turn needs no more than f32 precision"
)]
pub(crate) fn spin(elapsed: Duration) -> f32 {
    (elapsed.as_secs_f64() / TURN.as_secs_f64()).fract() as f32
}

/// Overlay state of one popup.
#[derive(Debug, Clone, Copy)]
struct Motion {
    mark: Mark,
    /// Mark shown before the last change; `None` until the mark changes.
    previous: Option<Mark>,
    changed: Instant,
    /// Start of the spinner's rotation.
    spinning: Instant,
    /// Time of the last frame returned.
    drawn: Instant,
}

impl Motion {
    fn new(state: RequestState, now: Instant) -> Self {
        Self {
            mark: Mark::of(state),
            previous: None,
            changed: now,
            spinning: now,
            drawn: now,
        }
    }

    /// Starts the transition to the mark of `state` at `now`, unless it is
    /// shown already; a new spinner starts at rest.
    fn set(&mut self, state: RequestState, now: Instant) {
        let mark = Mark::of(state);
        if mark == self.mark {
            return;
        }
        self.previous = Some(self.mark);
        self.mark = mark;
        self.changed = now;
        if mark == Mark::Spinner {
            self.spinning = now;
        }
    }

    /// Reports whether the next frame differs from the last one returned:
    /// the spinner turns, or the transition has not been drawn complete.
    fn moving(&self) -> bool {
        let transition_end = self.changed.checked_add(TRANSITION);
        self.mark == Mark::Spinner
            || (self.previous.is_some() && transition_end.is_some_and(|end| self.drawn < end))
    }

    /// Returns the frame at `now`, or the settled frame without `animate`.
    fn frame(&mut self, now: Instant, animate: bool) -> Frame {
        self.drawn = now;
        if !animate {
            return Frame::settled(self.mark);
        }
        let progress = match self.previous {
            Some(_) => progress(now.saturating_duration_since(self.changed)),
            None => 1.0,
        };
        Frame {
            mark: self.mark,
            previous: self.previous.filter(|_| progress < 1.0),
            progress,
            spin: spin(now.saturating_duration_since(self.spinning)),
        }
    }
}

/// Overlay states of the shown popups and the frames they need.
#[derive(Debug)]
pub(crate) struct Animations {
    /// Without it every frame is settled and none is scheduled.
    animate: bool,
    motions: BTreeMap<RequestId, Motion>,
}

impl Animations {
    pub(crate) fn new(animate: bool) -> Self {
        Self {
            animate,
            motions: BTreeMap::new(),
        }
    }

    /// Records `state` as shown for `id` at `now` and returns the frame to
    /// draw; a changed state starts a transition.
    pub(crate) fn show(&mut self, id: RequestId, state: RequestState, now: Instant) -> Frame {
        let animate = self.animate;
        self.motions
            .entry(id)
            .and_modify(|motion| motion.set(state, now))
            .or_insert_with(|| Motion::new(state, now))
            .frame(now, animate)
    }

    /// Returns the frame of `id` at `now`; `None` for an unknown id.
    pub(crate) fn frame(&mut self, id: RequestId, now: Instant) -> Option<Frame> {
        let animate = self.animate;
        self.motions
            .get_mut(&id)
            .map(|motion| motion.frame(now, animate))
    }

    /// Forgets `id`.
    pub(crate) fn remove(&mut self, id: RequestId) {
        self.motions.remove(&id);
    }

    /// Forgets every request `keep` rejects.
    pub(crate) fn retain(&mut self, keep: impl Fn(RequestId) -> bool) {
        self.motions.retain(|id, _| keep(*id));
    }

    /// Forgets every request.
    pub(crate) fn clear(&mut self) {
        self.motions.clear();
    }

    /// Returns when the next frame is due; `None` while nothing moves.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if !self.animate {
            return None;
        }
        self.motions
            .values()
            .filter(|motion| motion.moving())
            .map(|motion| {
                motion
                    .drawn
                    .checked_add(FRAME_INTERVAL)
                    .unwrap_or(motion.drawn)
            })
            .min()
    }

    /// Returns the frames at `now` of every popup that moves, once the
    /// earliest of them is due; none before that.
    pub(crate) fn due(&mut self, now: Instant) -> Vec<(RequestId, Frame)> {
        if self.next_deadline().is_none_or(|due| due > now) {
            return Vec::new();
        }
        let animate = self.animate;
        self.motions
            .iter_mut()
            .filter(|(_, motion)| motion.moving())
            .map(|(id, motion)| (*id, motion.frame(now, animate)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: RequestId = RequestId(1);
    const TOUCHED: RequestState = RequestState::Lingering(EndReason::Touched);

    fn after(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn each_state_has_its_mark() {
        assert_eq!(Mark::of(RequestState::Waiting), Mark::Spinner);
        assert_eq!(Mark::of(TOUCHED), Mark::Check);
        for reason in [EndReason::Cancelled, EndReason::Failed] {
            assert_eq!(Mark::of(RequestState::Lingering(reason)), Mark::Cross);
        }
        assert_eq!(
            Mark::of(RequestState::Lingering(EndReason::TimedOut)),
            Mark::Clock
        );
    }

    #[test]
    fn progress_eases_out_and_is_clamped() {
        assert!(progress(Duration::ZERO).abs() < f32::EPSILON);
        let half = progress(TRANSITION / 2);
        assert!((half - 0.875).abs() < 1e-6, "{half}");
        assert!((progress(TRANSITION) - 1.0).abs() < f32::EPSILON);
        assert!((progress(Duration::from_secs(3600)) - 1.0).abs() < f32::EPSILON);
        let steps: Vec<f32> = (0..=10).map(|ms| progress(TRANSITION * ms / 10)).collect();
        assert!(steps.windows(2).all(|pair| pair[0] < pair[1]), "{steps:?}");
    }

    #[test]
    fn spin_wraps_every_turn() {
        assert!(spin(Duration::ZERO).abs() < f32::EPSILON);
        assert!((spin(TURN / 4) - 0.25).abs() < 1e-6);
        assert!((spin(TURN * 3 + TURN / 2) - 0.5).abs() < 1e-6);
        let long = spin(Duration::from_hours(24 * 365));
        assert!((0.0..1.0).contains(&long), "{long}");
    }

    #[test]
    fn without_animation_frames_are_settled_and_unscheduled() {
        let t0 = Instant::now();
        let mut animations = Animations::new(false);
        let waiting = animations.show(ID, RequestState::Waiting, t0);
        assert_eq!(waiting, Frame::settled(Mark::Spinner));
        assert_eq!(animations.next_deadline(), None);
        let touched = animations.show(ID, TOUCHED, after(t0, 10));
        assert_eq!(touched, Frame::settled(Mark::Check));
        assert_eq!(animations.next_deadline(), None);
        assert_eq!(animations.due(after(t0, 1000)), []);
    }

    #[test]
    fn spinner_frames_repeat_until_hidden() {
        let t0 = Instant::now();
        let mut animations = Animations::new(true);
        assert_eq!(animations.next_deadline(), None);
        let first = animations.show(ID, RequestState::Waiting, t0);
        assert_eq!(first.previous, None);
        assert!((first.progress - 1.0).abs() < f32::EPSILON);
        let due = t0 + FRAME_INTERVAL;
        assert_eq!(animations.next_deadline(), Some(due));
        assert_eq!(animations.due(after(t0, 10)), []);
        let frames = animations.due(due);
        assert_eq!(frames.len(), 1);
        assert_eq!(animations.next_deadline(), Some(due + FRAME_INTERVAL));
        animations.remove(ID);
        assert_eq!(animations.next_deadline(), None);
    }

    #[test]
    fn outcome_transition_ends_with_a_settled_frame() {
        let t0 = Instant::now();
        let mut animations = Animations::new(true);
        animations.show(ID, RequestState::Waiting, t0);
        let start = animations.show(ID, TOUCHED, after(t0, 100));
        assert_eq!(start.mark, Mark::Check);
        assert_eq!(start.previous, Some(Mark::Spinner));
        assert!(start.progress.abs() < f32::EPSILON);
        let middle = animations.frame(ID, after(t0, 225));
        assert!(
            middle.is_some_and(|frame| frame.previous == Some(Mark::Spinner)
                && frame.progress > 0.0
                && frame.progress < 1.0)
        );
        assert!(animations.next_deadline().is_some());
        let end = animations.frame(ID, after(t0, 400));
        assert!(end.is_some_and(|frame| frame.mark == Mark::Check
            && frame.previous.is_none()
            && (frame.progress - 1.0).abs() < f32::EPSILON));
        assert_eq!(animations.next_deadline(), None);
    }

    #[test]
    fn retry_returns_to_a_spinner_at_rest() {
        let t0 = Instant::now();
        let mut animations = Animations::new(true);
        animations.show(ID, TOUCHED, t0);
        assert_eq!(animations.next_deadline(), None);
        let retry = animations.show(ID, RequestState::Waiting, after(t0, 500));
        assert_eq!(retry.mark, Mark::Spinner);
        assert_eq!(retry.previous, Some(Mark::Check));
        assert!(retry.spin.abs() < f32::EPSILON);
        assert!(animations.next_deadline().is_some());
    }

    #[test]
    fn unchanged_state_starts_no_transition() {
        let t0 = Instant::now();
        let mut animations = Animations::new(true);
        animations.show(ID, TOUCHED, t0);
        let again = animations.show(ID, TOUCHED, after(t0, 50));
        assert_eq!((again.mark, again.previous), (Mark::Check, None));
        assert!((again.progress - 1.0).abs() < f32::EPSILON);
        assert_eq!(animations.next_deadline(), None);
    }

    #[test]
    fn forgotten_requests_need_no_frames() {
        let t0 = Instant::now();
        let mut animations = Animations::new(true);
        animations.show(ID, RequestState::Waiting, t0);
        animations.show(RequestId(2), RequestState::Waiting, t0);
        animations.retain(|id| id == ID);
        assert_eq!(animations.frame(RequestId(2), t0), None);
        assert!(animations.next_deadline().is_some());
        animations.clear();
        assert_eq!(animations.next_deadline(), None);
        assert_eq!(animations.due(after(t0, 1000)), []);
    }
}
