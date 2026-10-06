//! Request state machine that turns signals into touch request lifecycles.
//!
//! The machine is pure: callers pass the current time to every call and call
//! [`Machine::tick`] no later than [`Machine::next_deadline`].

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::model::{Device, Method, Op, Outcome, Signal, SignalClass, SignalKind, Source};

/// Longest delay the machine schedules; longer configured delays are capped.
const MAX_DELAY: Duration = Duration::from_secs(600);

/// Timing of the request state machine. Both delays are capped at 600 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineConfig {
    /// Time a waiting request survives without a refreshing signal.
    pub keepalive_timeout: Duration,
    /// Time an ended operation lingers so that a client retry continues the same request.
    pub retry_window: Duration,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            keepalive_timeout: Duration::from_millis(1500),
            retry_window: Duration::from_millis(1000),
        }
    }
}

/// Identifier of a request, unique for the lifetime of one [`Machine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Why a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndReason {
    Touched,
    Cancelled,
    Failed,
    TimedOut,
}

/// Lifecycle state of an active request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestState {
    /// The device waits for a touch.
    Waiting,
    /// The operation ended and the request ends with this reason unless a
    /// client retry revives it within the retry window.
    ///
    /// A request may move from waiting to lingering and back within
    /// milliseconds, so a lingering request stays visible until
    /// [`Event::Ended`].
    Lingering(EndReason),
}

impl RequestState {
    /// Returns the name used in templates: `waiting`, `touched`, `cancelled`,
    /// `failed` or `timed_out`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Lingering(EndReason::Touched) => "touched",
            Self::Lingering(EndReason::Cancelled) => "cancelled",
            Self::Lingering(EndReason::Failed) => "failed",
            Self::Lingering(EndReason::TimedOut) => "timed_out",
        }
    }
}

/// A wait for a touch, possibly spanning several coalesced client retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: RequestId,
    pub device: Device,
    pub source: Source,
    pub class: SignalClass,
    pub method: Method,
    pub op: Option<Op>,
    /// Channel of the latest pending signal; progress and resolved signals
    /// apply only on this channel. The last pending channel wins, so a
    /// resolution on an earlier channel is ignored.
    pub channel: Option<u32>,
    /// Client process IDs, sorted and deduplicated.
    pub pids: Vec<u32>,
    pub started: Instant,
    /// Number of client attempts coalesced into this request, starting at 1.
    pub count: u32,
    /// Untrusted free text about the operation, such as an ssh key
    /// fingerprint and destination; never logged.
    pub detail: Option<String>,
    /// State at the time of the event carrying this request.
    pub state: RequestState,
}

/// Change in the set of active requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started(Request),
    /// The request changed, including entering or leaving the lingering state.
    Updated(Request),
    /// The request ended; `request.state` is `Lingering(reason)`.
    Ended {
        request: Request,
        reason: EndReason,
    },
}

#[derive(Debug)]
struct Entry {
    request: Request,
    deadline: Instant,
}

impl Entry {
    fn is_lingering(&self) -> bool {
        matches!(self.request.state, RequestState::Lingering(_))
    }

    /// Merges `pids` into the request and reports whether they changed.
    fn merge_pids(&mut self, pids: Vec<u32>) -> bool {
        if pids.is_empty() {
            return false;
        }
        let merged = union(&self.request.pids, pids);
        let changed = merged != self.request.pids;
        self.request.pids = merged;
        changed
    }
}

/// Tracks active requests, at most one per device and source.
///
/// An authenticator processes one transaction at a time, so pending signals
/// on any channel of a device and source belong to the same request. Progress
/// and resolved signals apply only on the request's current channel; signals
/// for other devices, sources or channels never change a request.
#[derive(Debug)]
pub struct Machine {
    cfg: MachineConfig,
    entries: BTreeMap<RequestId, Entry>,
    next_id: u64,
}

impl Machine {
    /// Creates a machine with no active requests.
    #[must_use]
    pub fn new(cfg: MachineConfig) -> Self {
        Self {
            cfg,
            entries: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Applies one signal observed at `now` and returns the resulting events.
    ///
    /// A pending signal starts a request, or refreshes the existing request of
    /// the same device and source and moves it to the signal's channel; the
    /// last pending channel wins. A lingering request is revived with its
    /// count incremented. A waiting request emits [`Event::Updated`] only when
    /// its method, op or pids change; a channel change alone is carried on the
    /// next event. A progress signal extends the deadline of a waiting or
    /// lingering request without changing its state. Non-empty signal pids are
    /// unioned into the request; use [`Machine::set_pids`] to replace them.
    pub fn handle(&mut self, signal: Signal, now: Instant) -> Vec<Event> {
        let found = self
            .entries
            .iter()
            .find(|(_, e)| {
                e.request.device.id == signal.device.id && e.request.source == signal.source
            })
            .map(|(id, _)| *id);
        if let SignalKind::Pending { method, op } = signal.kind {
            return match found {
                Some(id) => self.refresh(id, signal, method, op, now),
                None => self.start(signal, method, op, now),
            };
        }
        let Some(id) = found.filter(|id| {
            self.entries
                .get(id)
                .is_some_and(|e| e.request.channel == signal.channel)
        }) else {
            return Vec::new();
        };
        match signal.kind {
            SignalKind::Resolved(outcome) => self.resolve(id, outcome, signal.pids, now),
            SignalKind::Pending { .. } | SignalKind::Progress => {
                self.progress(id, signal.pids, now)
            }
        }
    }

    /// Replaces the pids of an active request with `pids`, sorted and deduplicated.
    ///
    /// Intended for attribution after [`Event::Started`] and after a revival,
    /// and when the client of a request changes.
    ///
    /// Returns [`Event::Updated`] if the pids changed, and `None` if they did
    /// not or no request has this `id`.
    pub fn set_pids(&mut self, id: RequestId, pids: Vec<u32>) -> Option<Event> {
        let entry = self.entries.get_mut(&id)?;
        let pids = union(&[], pids);
        if pids == entry.request.pids {
            return None;
        }
        entry.request.pids = pids;
        Some(Event::Updated(entry.request.clone()))
    }

    /// Replaces the detail of an active request.
    ///
    /// Returns [`Event::Updated`] if the detail changed, and `None` if it did
    /// not or no request has this `id`.
    pub fn set_detail(&mut self, id: RequestId, detail: Option<String>) -> Option<Event> {
        let entry = self.entries.get_mut(&id)?;
        if entry.request.detail == detail {
            return None;
        }
        entry.request.detail = detail;
        Some(Event::Updated(entry.request.clone()))
    }

    /// Applies every deadline that passed at `now` and returns the resulting events.
    ///
    /// Expired waiting requests linger with [`EndReason::TimedOut`] and emit
    /// [`Event::Updated`]; expired lingering requests end. Each request
    /// changes at most once per call, and events are ordered by [`RequestId`].
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        let linger = after(now, self.cfg.retry_window);
        let mut events = Vec::new();
        let mut ended = Vec::new();
        for (id, entry) in &mut self.entries {
            if entry.deadline > now {
                continue;
            }
            match entry.request.state {
                RequestState::Waiting => {
                    entry.request.state = RequestState::Lingering(EndReason::TimedOut);
                    entry.deadline = linger;
                    events.push(Event::Updated(entry.request.clone()));
                }
                RequestState::Lingering(reason) => {
                    ended.push(*id);
                    events.push(Event::Ended {
                        request: entry.request.clone(),
                        reason,
                    });
                }
            }
        }
        for id in ended {
            self.entries.remove(&id);
        }
        events
    }

    /// Returns the earliest instant at which [`Machine::tick`] changes state.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.values().map(|e| e.deadline).min()
    }

    /// Returns the requests that have started and not ended, in [`RequestId`] order.
    pub fn active(&self) -> impl Iterator<Item = &Request> {
        self.entries.values().map(|e| &e.request)
    }

    fn refresh(
        &mut self,
        id: RequestId,
        signal: Signal,
        method: Method,
        op: Option<Op>,
        now: Instant,
    ) -> Vec<Event> {
        let deadline = after(now, self.cfg.keepalive_timeout);
        let Some(entry) = self.entries.get_mut(&id) else {
            return Vec::new();
        };
        let revived = entry.is_lingering();
        let pids_changed = entry.merge_pids(signal.pids);
        let request = &mut entry.request;
        let changed = pids_changed || method != request.method || op != request.op;
        entry.deadline = deadline;
        request.state = RequestState::Waiting;
        request.channel = signal.channel;
        request.method = method;
        request.op = op;
        if revived {
            request.device = signal.device;
            request.class = signal.class;
            request.count = request.count.saturating_add(1);
        }
        if revived || changed {
            vec![Event::Updated(request.clone())]
        } else {
            Vec::new()
        }
    }

    fn start(
        &mut self,
        signal: Signal,
        method: Method,
        op: Option<Op>,
        now: Instant,
    ) -> Vec<Event> {
        let id = RequestId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        let request = Request {
            id,
            device: signal.device,
            source: signal.source,
            class: signal.class,
            method,
            op,
            channel: signal.channel,
            pids: union(&[], signal.pids),
            started: now,
            count: 1,
            detail: None,
            state: RequestState::Waiting,
        };
        self.entries.insert(
            id,
            Entry {
                request: request.clone(),
                deadline: after(now, self.cfg.keepalive_timeout),
            },
        );
        vec![Event::Started(request)]
    }

    fn progress(&mut self, id: RequestId, pids: Vec<u32>, now: Instant) -> Vec<Event> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Vec::new();
        };
        let delay = if entry.is_lingering() {
            self.cfg.retry_window
        } else {
            self.cfg.keepalive_timeout
        };
        entry.deadline = after(now, delay);
        if entry.merge_pids(pids) {
            vec![Event::Updated(entry.request.clone())]
        } else {
            Vec::new()
        }
    }

    fn resolve(
        &mut self,
        id: RequestId,
        outcome: Outcome,
        pids: Vec<u32>,
        now: Instant,
    ) -> Vec<Event> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Vec::new();
        };
        let pids_changed = entry.merge_pids(pids);
        let reason = match outcome {
            Outcome::Touched => {
                return self
                    .entries
                    .remove(&id)
                    .map(|mut e| {
                        e.request.state = RequestState::Lingering(EndReason::Touched);
                        Event::Ended {
                            request: e.request,
                            reason: EndReason::Touched,
                        }
                    })
                    .into_iter()
                    .collect();
            }
            Outcome::Cancelled => EndReason::Cancelled,
            Outcome::Failed => EndReason::Failed,
            Outcome::TimedOut => EndReason::TimedOut,
        };
        let state = RequestState::Lingering(reason);
        entry.deadline = after(now, self.cfg.retry_window);
        if entry.request.state == state && !pids_changed {
            return Vec::new();
        }
        entry.request.state = state;
        vec![Event::Updated(entry.request.clone())]
    }
}

/// Returns `now + min(delay, MAX_DELAY)`, or `now` if even that is not representable.
fn after(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay.min(MAX_DELAY)).unwrap_or(now)
}

fn union(current: &[u32], added: Vec<u32>) -> Vec<u32> {
    let mut pids = added;
    pids.extend_from_slice(current);
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctaphid::{self, CTAPHID_CBOR, CTAPHID_KEEPALIVE, CTAPHID_MSG, Frame, UPNEEDED};
    use crate::model::{DeviceId, DeviceKind, Transport};

    use crate::test_error::TestResult;

    const MS: Duration = Duration::from_millis(1);
    const NONE: [Event; 0] = [];

    fn device(id: &str) -> Device {
        Device {
            id: DeviceId(id.to_owned()),
            kind: DeviceKind::Fido,
            transport: Transport::Usb,
            vid: Some(0x1050),
            pid: Some(0x0407),
            vendor: Some("Yubico".to_owned()),
            model: None,
            product: None,
        }
    }

    fn signal(dev: &str, channel: u32, kind: SignalKind, pids: &[u32]) -> Signal {
        Signal {
            device: device(dev),
            source: Source::Fido,
            class: SignalClass::Asserted,
            kind,
            channel: Some(channel),
            pids: pids.to_vec(),
        }
    }

    fn pending(method: Method) -> SignalKind {
        SignalKind::Pending { method, op: None }
    }

    fn resolved(outcome: Outcome) -> SignalKind {
        SignalKind::Resolved(outcome)
    }

    fn report(cid: u32, cmd: u8, data: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..4].copy_from_slice(&cid.to_be_bytes());
        r[4] = cmd;
        // The low two bytes of the length, big-endian.
        let len = data.len().to_be_bytes();
        r[5..7].copy_from_slice(&len[len.len() - 2..]);
        r[7..7 + data.len()].copy_from_slice(data);
        r
    }

    /// Maps a raw report to a signal the way a FIDO detector would.
    fn from_report(dev: &str, raw: &[u8]) -> Option<Signal> {
        let (cid, frame) = ctaphid::parse(raw)?;
        let kind = match frame {
            Frame::KeepaliveUpNeeded => pending(Method::Fido2),
            Frame::U2fConditionsNotSatisfied => pending(Method::U2f),
            Frame::KeepaliveProcessing => SignalKind::Progress,
            Frame::Done(outcome) => resolved(outcome),
            Frame::Other => return None,
        };
        Some(signal(dev, cid, kind, &[42]))
    }

    /// Drives the machine in 10 ms steps up to `end_ms`, feeding the reports
    /// due at each step, and returns every event with its time in ms.
    fn run(
        m: &mut Machine,
        base: Instant,
        end_ms: u32,
        reports: impl Fn(u32) -> Vec<Vec<u8>>,
    ) -> Vec<(u32, Event)> {
        let mut log = Vec::new();
        for t in (0..=end_ms).step_by(10) {
            let now = base + MS * t;
            for raw in reports(t) {
                if let Some(s) = from_report("yk", &raw) {
                    log.extend(m.handle(s, now).into_iter().map(|e| (t, e)));
                }
            }
            log.extend(m.tick(now).into_iter().map(|e| (t, e)));
        }
        log
    }

    fn lingers(events: &[Event], reason: EndReason) -> bool {
        matches!(events, [Event::Updated(r)] if r.state == RequestState::Lingering(reason))
    }

    #[test]
    fn keepalives_on_one_channel_ignore_other_channels() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 6000, |t| {
            let mut raws = Vec::new();
            if t <= 2900 && t.is_multiple_of(100) {
                raws.push(report(1, CTAPHID_KEEPALIVE, &[UPNEEDED]));
            }
            if t <= 3000 && t % 100 == 50 {
                raws.push(report(2, CTAPHID_CBOR, &[0x00]));
                raws.push(report(2, 0x86, &[0; 17]));
            }
            raws
        });

        let [
            (0, Event::Started(started)),
            (4400, Event::Updated(lingering)),
            (5400, Event::Ended { request, reason }),
        ] = log.as_slice()
        else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(started.channel, Some(1));
        assert_eq!(
            lingering.state,
            RequestState::Lingering(EndReason::TimedOut)
        );
        assert_eq!(request.id, started.id);
        assert_eq!(*reason, EndReason::TimedOut);
        assert_eq!(m.active().count(), 0);
        Ok(())
    }

    #[test]
    fn cancelled_cbor_then_retry_on_new_channel_continues_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 2000, |t| match t {
            0..=500 if t.is_multiple_of(100) => vec![report(1, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            // CTAP2_ERR_KEEPALIVE_CANCEL
            550 => vec![report(1, CTAPHID_CBOR, &[0x2d])],
            1200..=2000 if t.is_multiple_of(100) => vec![report(2, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            _ => Vec::new(),
        });

        let [
            (0, Event::Started(started)),
            (550, Event::Updated(cancelled)),
            (1200, Event::Updated(retried)),
        ] = log.as_slice()
        else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(
            cancelled.state,
            RequestState::Lingering(EndReason::Cancelled)
        );
        assert_eq!(retried.id, started.id);
        assert_eq!(retried.count, 2);
        assert_eq!(retried.channel, Some(2));
        assert_eq!(retried.state, RequestState::Waiting);
        Ok(())
    }

    #[test]
    fn touched_ends_immediately() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let started = m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        assert!(matches!(started.as_slice(), [Event::Started(_)]));
        let ended = m.handle(
            signal("yk", 1, resolved(Outcome::Touched), &[]),
            base + MS * 300,
        );
        assert!(matches!(
            ended.as_slice(),
            [Event::Ended {
                reason: EndReason::Touched,
                ..
            }]
        ));
        assert_eq!(m.active().count(), 0);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn u2f_retry_on_new_channel_continues_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let first = m.handle(signal("yk", 10, pending(Method::U2f), &[7]), base);
        let [Event::Started(started)] = first.as_slice() else {
            return Err("expected Started".into());
        };
        let failed = m.handle(
            signal("yk", 10, resolved(Outcome::Failed), &[7]),
            base + MS * 100,
        );
        assert!(lingers(&failed, EndReason::Failed));
        assert_eq!(m.tick(base + MS * 500), NONE);
        let retry = m.handle(
            signal("yk", 11, pending(Method::U2f), &[7]),
            base + MS * 900,
        );
        let [Event::Updated(updated)] = retry.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.id, started.id);
        assert_eq!(updated.count, 2);
        assert_eq!(updated.channel, Some(11));
        assert_eq!(updated.state, RequestState::Waiting);
        assert_eq!(m.tick(base + MS * 2000), NONE);
        assert_eq!(m.active().count(), 1);
        Ok(())
    }

    #[test]
    fn retry_on_same_channel_continues_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 10, pending(Method::U2f), &[7]), base);
        assert!(lingers(&m.tick(base + MS * 1500), EndReason::TimedOut));
        let retry = m.handle(
            signal("yk", 10, pending(Method::U2f), &[7]),
            base + MS * 1600,
        );
        let [Event::Updated(updated)] = retry.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.count, 2);
        assert_eq!(updated.state, RequestState::Waiting);
        assert_eq!(m.tick(base + MS * 2600), NONE);
        assert_eq!(m.next_deadline(), Some(base + MS * 3100));
        Ok(())
    }

    #[test]
    fn progress_extends_lingering_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), base);
        assert_eq!(
            m.handle(signal("yk", 1, SignalKind::Progress, &[]), base + MS * 900),
            NONE
        );
        assert_eq!(m.tick(base + MS * 1500), NONE);
        let retry = m.handle(
            signal("yk", 1, pending(Method::Fido2), &[]),
            base + MS * 1800,
        );
        let [Event::Updated(updated)] = retry.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.count, 2);
        Ok(())
    }

    #[test]
    fn repeated_resolve_reports_only_state_changes() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        let first = m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), base);
        assert!(lingers(&first, EndReason::Failed));
        assert_eq!(
            m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), base),
            NONE
        );
        let cancelled = m.handle(signal("yk", 1, resolved(Outcome::Cancelled), &[]), base);
        assert!(lingers(&cancelled, EndReason::Cancelled));
    }

    #[test]
    fn cancelled_ends_after_retry_window() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        let cancel = m.handle(
            signal("yk", 1, resolved(Outcome::Cancelled), &[]),
            base + MS * 200,
        );
        assert!(lingers(&cancel, EndReason::Cancelled));
        assert_eq!(m.tick(base + MS * 1199), NONE);
        let ended = m.tick(base + MS * 1200);
        assert!(matches!(
            ended.as_slice(),
            [Event::Ended {
                reason: EndReason::Cancelled,
                ..
            }]
        ));
    }

    #[test]
    fn cbor_status_sets_end_reason() -> TestResult {
        for (status, reason) in [
            (0x2d, EndReason::Cancelled),
            (0x27, EndReason::Cancelled),
            (0x2f, EndReason::TimedOut),
            (0x3a, EndReason::TimedOut),
            (0x31, EndReason::Failed),
        ] {
            let base = Instant::now();
            let mut m = Machine::new(MachineConfig::default());
            let log = run(&mut m, base, 2000, |t| match t {
                0 => vec![report(1, CTAPHID_KEEPALIVE, &[UPNEEDED])],
                200 => vec![report(1, CTAPHID_CBOR, &[status])],
                _ => Vec::new(),
            });
            let [
                (0, Event::Started(_)),
                (200, Event::Updated(lingering)),
                (1200, Event::Ended { reason: ended, .. }),
            ] = log.as_slice()
            else {
                return Err(format!("unexpected events for {status:#04x}: {log:?}").into());
            };
            assert_eq!(lingering.state, RequestState::Lingering(reason));
            assert_eq!(*ended, reason, "{status:#04x}");
        }
        Ok(())
    }

    #[test]
    fn devices_are_independent() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("a", 1, pending(Method::Fido2), &[1]), base);
        m.handle(signal("b", 1, pending(Method::Fido2), &[1]), base);
        let ended = m.handle(
            signal("b", 1, resolved(Outcome::Touched), &[1]),
            base + MS * 100,
        );
        let [Event::Ended { request, .. }] = ended.as_slice() else {
            return Err("expected one Ended".into());
        };
        assert_eq!(request.device.id, DeviceId("b".to_owned()));
        let left: Vec<&DeviceId> = m.active().map(|r| &r.device.id).collect();
        assert_eq!(left, [&DeviceId("a".to_owned())]);
        Ok(())
    }

    #[test]
    fn progress_keeps_request_alive() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        for step in 1..=10u32 {
            let now = base + MS * (step * 1000);
            assert_eq!(
                m.handle(signal("yk", 1, SignalKind::Progress, &[]), now),
                NONE
            );
            assert_eq!(m.tick(now), NONE);
        }
        assert_eq!(m.active().count(), 1);
        assert_eq!(
            m.handle(signal("yk", 2, SignalKind::Progress, &[]), base),
            NONE
        );
        assert_eq!(m.active().count(), 1);
    }

    #[test]
    fn refresh_reports_only_changes() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[3]), base);
        assert_eq!(
            m.handle(
                signal("yk", 1, pending(Method::Fido2), &[3]),
                base + MS * 100
            ),
            NONE
        );
        let more = m.handle(
            signal("yk", 1, pending(Method::Fido2), &[5, 3]),
            base + MS * 200,
        );
        let [Event::Updated(updated)] = more.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.pids, [3, 5]);
        assert_eq!(updated.count, 1);
        Ok(())
    }

    #[test]
    fn next_deadline_tracks_earliest_state_change() {
        let base = Instant::now();
        let cfg = MachineConfig::default();
        let mut m = Machine::new(cfg);
        assert_eq!(m.next_deadline(), None);
        m.handle(signal("a", 1, pending(Method::Fido2), &[]), base);
        assert_eq!(m.next_deadline(), Some(base + cfg.keepalive_timeout));
        m.handle(signal("b", 1, pending(Method::Fido2), &[]), base + MS * 500);
        assert_eq!(m.next_deadline(), Some(base + cfg.keepalive_timeout));
        m.handle(signal("a", 1, pending(Method::Fido2), &[]), base + MS * 600);
        assert_eq!(
            m.next_deadline(),
            Some(base + MS * 500 + cfg.keepalive_timeout)
        );
        assert!(lingers(&m.tick(base + MS * 2000), EndReason::TimedOut));
        assert_eq!(m.next_deadline(), Some(base + MS * 2100));
    }

    #[test]
    fn simultaneous_expiries_are_ordered_by_request_id() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        for (dev, channel) in [("b", 1), ("a", 2), ("c", 3)] {
            m.handle(signal(dev, channel, pending(Method::Fido2), &[]), base);
        }
        let ids = |events: Vec<Event>| -> Vec<u64> {
            events
                .into_iter()
                .map(|e| match e {
                    Event::Started(r) | Event::Updated(r) | Event::Ended { request: r, .. } => {
                        r.id.0
                    }
                })
                .collect()
        };
        assert_eq!(ids(m.tick(base + MS * 1500)), [1, 2, 3]);
        let ended = m.tick(base + MS * 2500);
        assert!(ended.iter().all(|e| matches!(e, Event::Ended { .. })));
        assert_eq!(ids(ended), [1, 2, 3]);
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let earlier = Instant::now();
        let base = earlier + Duration::from_secs(5);
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        assert_eq!(m.tick(earlier), NONE);
        assert_eq!(
            m.handle(signal("yk", 1, pending(Method::Fido2), &[]), earlier),
            NONE
        );
        m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), earlier);
        let ended = m.tick(base);
        assert!(matches!(
            ended.as_slice(),
            [Event::Ended {
                reason: EndReason::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn huge_durations_are_capped() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig {
            keepalive_timeout: Duration::MAX,
            retry_window: Duration::MAX,
        });
        let started = m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        assert!(matches!(started.as_slice(), [Event::Started(_)]));
        assert_eq!(m.next_deadline(), Some(base + MAX_DELAY));
        assert!(lingers(&m.tick(base + MAX_DELAY), EndReason::TimedOut));
        assert_eq!(m.next_deadline(), Some(base + MAX_DELAY * 2));
        assert!(matches!(
            m.tick(base + MAX_DELAY * 2).as_slice(),
            [Event::Ended { .. }]
        ));
    }

    #[test]
    fn unknown_key_resolve_and_progress_are_ignored() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        assert_eq!(
            m.handle(signal("yk", 9, resolved(Outcome::Touched), &[]), base),
            NONE
        );
        assert_eq!(
            m.handle(signal("yk", 9, SignalKind::Progress, &[]), base),
            NONE
        );
        assert_eq!(m.active().count(), 0);
    }

    #[test]
    fn u2f_polling_then_touch_is_one_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 3500, |t| match t {
            0..3000 if t.is_multiple_of(200) => vec![report(5, CTAPHID_MSG, &[0x69, 0x85])],
            3000 => vec![report(5, CTAPHID_MSG, &[0x01, 0x90, 0x00])],
            _ => Vec::new(),
        });
        let [
            (0, Event::Started(started)),
            (3000, Event::Ended { request, reason }),
        ] = log.as_slice()
        else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(started.method, Method::U2f);
        assert_eq!(request.id, started.id);
        assert_eq!(*reason, EndReason::Touched);
        Ok(())
    }

    #[test]
    fn revival_after_timeout_keeps_identity() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[7]), base);
        assert!(lingers(&m.tick(base + MS * 1500), EndReason::TimedOut));
        let retry = m.handle(
            signal("yk", 2, pending(Method::Fido2), &[7]),
            base + MS * 2000,
        );
        let [Event::Updated(updated)] = retry.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.id, RequestId(1));
        assert_eq!(updated.count, 2);
        assert_eq!(updated.started, base);
        assert_eq!(updated.state, RequestState::Waiting);
        Ok(())
    }

    #[test]
    fn lingering_deadline_follows_last_signal() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.handle(
            signal("yk", 1, resolved(Outcome::Failed), &[]),
            base + MS * 100,
        );
        assert_eq!(m.next_deadline(), Some(base + MS * 1100));
        m.handle(
            signal("yk", 1, resolved(Outcome::Cancelled), &[]),
            base + MS * 400,
        );
        assert_eq!(m.next_deadline(), Some(base + MS * 1400));
        m.handle(signal("yk", 1, SignalKind::Progress, &[]), base + MS * 700);
        assert_eq!(m.next_deadline(), Some(base + MS * 1700));
    }

    #[test]
    fn touched_ends_lingering_request() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), base);
        let ended = m.handle(
            signal("yk", 1, resolved(Outcome::Touched), &[]),
            base + MS * 100,
        );
        assert!(matches!(
            ended.as_slice(),
            [Event::Ended {
                reason: EndReason::Touched,
                ..
            }]
        ));
        assert_eq!(m.active().count(), 0);
    }

    #[test]
    fn changed_method_or_op_is_reported() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        let method = m.handle(signal("yk", 1, pending(Method::U2f), &[]), base);
        let [Event::Updated(updated)] = method.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.method, Method::U2f);
        let op = SignalKind::Pending {
            method: Method::U2f,
            op: Some(Op::Register),
        };
        let op = m.handle(signal("yk", 1, op, &[]), base);
        let [Event::Updated(updated)] = op.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.op, Some(Op::Register));
        Ok(())
    }

    #[test]
    fn sources_on_one_device_are_independent() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let gpg = |kind| Signal {
            source: Source::Gpg,
            ..signal("yk", 1, kind, &[])
        };
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.handle(gpg(pending(Method::OpenPgp)), base);
        let ended = m.handle(gpg(resolved(Outcome::Touched)), base);
        let [Event::Ended { request, .. }] = ended.as_slice() else {
            return Err("expected one Ended".into());
        };
        assert_eq!(request.source, Source::Gpg);
        let left: Vec<Source> = m.active().map(|r| r.source).collect();
        assert_eq!(left, [Source::Fido]);
        Ok(())
    }

    #[test]
    fn requests_without_channel_are_keyed() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let unchanneled = |kind| Signal {
            channel: None,
            ..signal("yk", 0, kind, &[])
        };
        let started = m.handle(unchanneled(pending(Method::Piv)), base);
        assert!(matches!(started.as_slice(), [Event::Started(r)] if r.channel.is_none()));
        assert_eq!(
            m.handle(unchanneled(pending(Method::Piv)), base + MS * 100),
            NONE
        );
        assert_eq!(
            m.handle(signal("yk", 0, resolved(Outcome::Touched), &[]), base),
            NONE
        );
        let ended = m.handle(unchanneled(resolved(Outcome::Touched)), base + MS * 200);
        assert!(matches!(ended.as_slice(), [Event::Ended { .. }]));
    }

    #[test]
    fn identical_input_gives_identical_events() {
        let base = Instant::now();
        let reports = |t: u32| match t {
            0..=900 if t.is_multiple_of(100) => vec![report(1, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            300..=700 if t % 100 == 50 => vec![report(2, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            950 => vec![report(1, CTAPHID_CBOR, &[0x2d])],
            _ => Vec::new(),
        };
        let first = run(
            &mut Machine::new(MachineConfig::default()),
            base,
            4000,
            reports,
        );
        let second = run(
            &mut Machine::new(MachineConfig::default()),
            base,
            4000,
            reports,
        );
        assert_eq!(first.len(), 3);
        assert_eq!(first, second);
    }

    #[test]
    fn polling_on_fresh_channels_is_one_request() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 1000, |t| match t {
            0 | 300 | 600 => vec![report(t / 300 + 1, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            _ => Vec::new(),
        });
        let [(0, Event::Started(started))] = log.as_slice() else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(started.channel, Some(1));
        let current: Vec<(RequestId, Option<u32>, u32)> =
            m.active().map(|r| (r.id, r.channel, r.count)).collect();
        assert_eq!(current, [(started.id, Some(3), 1)]);
        Ok(())
    }

    #[test]
    fn two_u2f_pollers_emit_only_started() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 3000, |t| match t % 200 {
            0 => vec![report(1, CTAPHID_MSG, &[0x69, 0x85])],
            100 => vec![report(2, CTAPHID_MSG, &[0x69, 0x85])],
            _ => Vec::new(),
        });
        let [(0, Event::Started(_))] = log.as_slice() else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(m.active().count(), 1);
        Ok(())
    }

    #[test]
    fn last_pending_channel_wins() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let log = run(&mut m, base, 2500, |t| match t {
            0..=500 if t.is_multiple_of(100) => vec![report(1, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            550 => vec![report(2, CTAPHID_KEEPALIVE, &[UPNEEDED])],
            600 => vec![report(1, CTAPHID_CBOR, &[0x00])],
            _ => Vec::new(),
        });
        let [(0, Event::Started(_)), (2050, Event::Updated(timed_out))] = log.as_slice() else {
            return Err(format!("unexpected events: {log:?}").into());
        };
        assert_eq!(timed_out.channel, Some(2));
        assert_eq!(
            timed_out.state,
            RequestState::Lingering(EndReason::TimedOut)
        );
        Ok(())
    }

    #[test]
    fn mismatched_channel_presence_is_ignored() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        let unchanneled = |dev, kind| Signal {
            channel: None,
            ..signal(dev, 0, kind, &[])
        };
        m.handle(signal("a", 1, pending(Method::Fido2), &[]), base);
        assert_eq!(
            m.handle(unchanneled("a", resolved(Outcome::Touched)), base),
            NONE
        );
        m.handle(unchanneled("b", pending(Method::Fido2)), base);
        assert_eq!(
            m.handle(signal("b", 1, resolved(Outcome::Touched), &[]), base),
            NONE
        );
        assert_eq!(m.active().count(), 2);
    }

    #[test]
    fn ended_state_matches_reason() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        let touched = m.handle(signal("yk", 1, resolved(Outcome::Touched), &[]), base);
        let [Event::Ended { request, reason }] = touched.as_slice() else {
            return Err("expected Ended".into());
        };
        assert_eq!(request.state, RequestState::Lingering(*reason));

        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.tick(base + MS * 1500);
        let timed_out = m.tick(base + MS * 2500);
        let [Event::Ended { request, reason }] = timed_out.as_slice() else {
            return Err("expected Ended".into());
        };
        assert_eq!(*reason, EndReason::TimedOut);
        assert_eq!(request.state, RequestState::Lingering(*reason));
        Ok(())
    }

    #[test]
    fn set_pids_then_retry_unions_pids() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::U2f), &[]), base);
        m.handle(signal("yk", 1, resolved(Outcome::Failed), &[]), base);
        let Some(Event::Updated(attributed)) = m.set_pids(RequestId(1), vec![5]) else {
            return Err("expected Updated".into());
        };
        assert_eq!(attributed.state, RequestState::Lingering(EndReason::Failed));
        let retry = m.handle(signal("yk", 2, pending(Method::U2f), &[6]), base + MS * 100);
        let [Event::Updated(revived)] = retry.as_slice() else {
            return Err("expected Updated".into());
        };
        assert_eq!(revived.pids, [5, 6]);
        assert_eq!(revived.count, 2);
        assert_eq!(revived.state, RequestState::Waiting);
        Ok(())
    }

    #[test]
    fn resolve_on_stale_channel_is_ignored() {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        m.handle(
            signal("yk", 2, pending(Method::Fido2), &[]),
            base + MS * 100,
        );
        for outcome in [Outcome::Touched, Outcome::Cancelled, Outcome::Failed] {
            assert_eq!(
                m.handle(signal("yk", 1, resolved(outcome), &[]), base + MS * 200),
                NONE
            );
        }
        assert_eq!(
            m.handle(signal("yk", 1, SignalKind::Progress, &[]), base + MS * 1000),
            NONE
        );
        assert_eq!(m.next_deadline(), Some(base + MS * 1600));
        let ended = m.handle(
            signal("yk", 2, resolved(Outcome::Touched), &[]),
            base + MS * 300,
        );
        assert!(matches!(ended.as_slice(), [Event::Ended { .. }]));
    }

    #[test]
    fn set_pids_reports_change_once() -> TestResult {
        let base = Instant::now();
        let mut m = Machine::new(MachineConfig::default());
        m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base);
        let Some(Event::Updated(updated)) = m.set_pids(RequestId(1), vec![9, 4, 9]) else {
            return Err("expected Updated".into());
        };
        assert_eq!(updated.pids, [4, 9]);
        assert_eq!(m.set_pids(RequestId(1), vec![4, 9]), None);
        assert_eq!(m.set_pids(RequestId(2), vec![1]), None);
        assert_eq!(
            m.handle(signal("yk", 1, pending(Method::Fido2), &[]), base),
            NONE
        );
        Ok(())
    }
}
