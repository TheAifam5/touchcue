//! Dispatcher task that owns the active set and fans events out.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use touchcue_core::RequestState;

use crate::wire::{Kind, WireEvent};

/// Messages each client may fall behind before it is dropped.
pub(crate) const CLIENT_BUFFER: usize = 64;
/// Most requests tracked at once; starts beyond it are dropped.
const MAX_ACTIVE: usize = 256;

/// Socket a client connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Json,
    Compat,
}

/// Work for the dispatcher task.
#[derive(Debug)]
pub(crate) enum Msg {
    Event(WireEvent),
    /// Replaces the active set with these requests.
    Resync(Vec<WireEvent>),
}

/// A client's request for its snapshot and live feed.
#[derive(Debug)]
pub(crate) struct Registration {
    pub(crate) endpoint: Endpoint,
    pub(crate) reply: oneshot::Sender<Subscription>,
}

/// Snapshot of the active requests and the feed of everything published after it.
#[derive(Debug)]
pub(crate) struct Subscription {
    pub(crate) snapshot: Vec<u8>,
    pub(crate) feed: broadcast::Receiver<Arc<[u8]>>,
}

/// Feed of the D-Bus task: every event, plus the active set to repair from
/// after a lag. The active set is updated before the event is sent.
#[derive(Debug)]
pub(crate) struct BusFeed {
    pub(crate) events: broadcast::Sender<WireEvent>,
    pub(crate) active: watch::Sender<Arc<Vec<WireEvent>>>,
}

/// Group of sources the max-baz compat socket reports together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compat {
    U2f,
    Gpg,
    Mac,
}

impl Compat {
    fn of(source: &str) -> Option<Compat> {
        match source {
            "fido" => Some(Compat::U2f),
            "gpg" | "ssh" => Some(Compat::Gpg),
            "hmac" => Some(Compat::Mac),
            _ => None,
        }
    }

    fn message(self, on: bool) -> &'static [u8; 5] {
        match (self, on) {
            (Compat::U2f, true) => b"U2F_1",
            (Compat::U2f, false) => b"U2F_0",
            (Compat::Gpg, true) => b"GPG_1",
            (Compat::Gpg, false) => b"GPG_0",
            (Compat::Mac, true) => b"MAC_1",
            (Compat::Mac, false) => b"MAC_0",
        }
    }
}

/// State owned by the dispatcher task.
#[derive(Debug)]
pub(crate) struct Dispatcher {
    /// Latest event of every active request.
    active: BTreeMap<u64, WireEvent>,
    json: broadcast::Sender<Arc<[u8]>>,
    compat: broadcast::Sender<Arc<[u8]>>,
    bus: Option<BusFeed>,
}

impl Dispatcher {
    pub(crate) fn new(bus: Option<BusFeed>) -> Dispatcher {
        Dispatcher {
            active: BTreeMap::new(),
            json: broadcast::channel(CLIENT_BUFFER).0,
            compat: broadcast::channel(CLIENT_BUFFER).0,
            bus,
        }
    }

    /// Handles registrations and messages until `events` closes or `cancel`
    /// fires. Messages queued before `events` closed are still handled.
    /// Returning drops the client and D-Bus feeds, which ends those tasks
    /// once they drained what was sent.
    pub(crate) async fn run(
        mut self,
        mut events: mpsc::Receiver<Msg>,
        mut registrations: mpsc::Receiver<Registration>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                Some(registration) = registrations.recv() => self.register(registration),
                msg = events.recv() => match msg {
                    Some(Msg::Event(ev)) => self.publish(&ev),
                    Some(Msg::Resync(active)) => self.resync(active),
                    None => break,
                },
            }
        }
        tracing::debug!(active = self.active.len(), "IPC dispatcher stopped");
    }

    /// Answers a registration with the snapshot and a feed subscribed at the
    /// same point, so the client sees every later event exactly once.
    fn register(&self, registration: Registration) {
        let Registration { endpoint, reply } = registration;
        let subscription = match endpoint {
            Endpoint::Json => Subscription {
                snapshot: self.json_snapshot(),
                feed: self.json.subscribe(),
            },
            Endpoint::Compat => Subscription {
                snapshot: self.compat_snapshot(),
                feed: self.compat.subscribe(),
            },
        };
        if let Err(_unsent) = reply.send(subscription) {
            tracing::debug!(?endpoint, "client left before its registration");
        }
    }

    /// Replaces the active set with `active`, publishing what changed: an
    /// `ended` event with no reason and the last state and values for each
    /// request that is gone, `started` for each new one, and `updated` for
    /// each changed one.
    #[tracing::instrument(level = "debug", skip_all, fields(active = active.len()))]
    fn resync(&mut self, active: Vec<WireEvent>) {
        let fresh: BTreeMap<u64, WireEvent> = active.into_iter().map(|ev| (ev.id, ev)).collect();
        let stale: Vec<WireEvent> = self
            .active
            .values()
            .filter(|ev| !fresh.contains_key(&ev.id))
            .map(|ev| WireEvent {
                kind: Kind::Ended,
                reason: None,
                ..ev.clone()
            })
            .collect();
        if !stale.is_empty() {
            tracing::info!(stale = stale.len(), "resync ends stale requests");
        }
        for ev in &stale {
            self.publish(ev);
        }
        for (id, ev) in fresh {
            let kind = match self.active.get(&id) {
                None => Kind::Started,
                Some(known) if known.state != ev.state || known.values != ev.values => {
                    Kind::Updated
                }
                Some(_) => continue,
            };
            self.publish(&WireEvent {
                kind,
                reason: None,
                ..ev
            });
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(id = ev.id, kind = ?ev.kind))]
    fn publish(&mut self, ev: &WireEvent) {
        let compat = Compat::of(&ev.source);
        let was_on = compat.map(|c| self.compat_on(c));
        match ev.kind {
            Kind::Started | Kind::Updated => {
                if !self.active.contains_key(&ev.id) && self.active.len() >= MAX_ACTIVE {
                    tracing::warn!(id = ev.id, "too many active requests, event dropped");
                    return;
                }
                self.active.insert(ev.id, ev.clone());
            }
            Kind::Ended => {
                self.active.remove(&ev.id);
            }
        }
        if let (Some(compat), Some(was_on)) = (compat, was_on) {
            let on = self.compat_on(compat);
            if on != was_on {
                send(Endpoint::Compat, &self.compat, &compat.message(on)[..]);
            }
        }
        if self.json.receiver_count() > 0 {
            match line(ev) {
                Ok(bytes) => send(Endpoint::Json, &self.json, &bytes),
                Err(error) => {
                    tracing::warn!(
                        id = ev.id,
                        error = &error as &dyn Error,
                        "cannot serialize event"
                    );
                }
            }
        }
        if let Some(bus) = &self.bus {
            bus.active
                .send_replace(Arc::new(self.active.values().cloned().collect()));
            if let Err(_unsent) = bus.events.send(ev.clone()) {
                tracing::debug!(id = ev.id, "D-Bus task stopped, event not sent");
            }
        }
    }

    /// Reports whether a request of `compat` waits for a touch; lingering
    /// requests do not count, as in the original tool.
    fn compat_on(&self, compat: Compat) -> bool {
        let waiting = RequestState::Waiting.as_str();
        self.active
            .values()
            .any(|ev| ev.state == waiting && Compat::of(&ev.source) == Some(compat))
    }

    fn json_snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in self.active.values() {
            let started = WireEvent {
                kind: Kind::Started,
                ..ev.clone()
            };
            match line(&started) {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(error) => {
                    tracing::warn!(
                        id = ev.id,
                        error = &error as &dyn Error,
                        "cannot serialize event"
                    );
                }
            }
        }
        out
    }

    fn compat_snapshot(&self) -> Vec<u8> {
        [Compat::U2f, Compat::Gpg, Compat::Mac]
            .into_iter()
            .filter(|c| self.compat_on(*c))
            .flat_map(|c| c.message(true).iter().copied())
            .collect()
    }
}

fn line(ev: &WireEvent) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(ev)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn send(endpoint: Endpoint, feed: &broadcast::Sender<Arc<[u8]>>, bytes: &[u8]) {
    if feed.receiver_count() > 0
        && let Err(_unsent) = feed.send(Arc::from(bytes))
    {
        tracing::debug!(?endpoint, "every client left before the send");
    }
}
