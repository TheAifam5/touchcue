//! Session bus service that mirrors the active requests.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use zbus::connection::Builder;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{Connection, interface};

use crate::IpcError;
use crate::wire::{Kind, WireEvent};

/// Well-known bus name; owning it marks the running instance.
pub(crate) const NAME: &str = "io.github.theaifam5.Touchcue";
const PATH: &str = "/io/github/theaifam5/Touchcue";
/// Upper bound on calls this service makes to the bus, such as releasing the name.
const METHOD_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound on connecting to the session bus, serving the object and acquiring [`NAME`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on releasing [`NAME`] and closing the connection.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

type Values = BTreeMap<String, String>;

struct Service {
    active: BTreeMap<u64, Values>,
}

#[interface(name = "io.github.theaifam5.Touchcue1")]
impl Service {
    /// True while any request is active.
    #[zbus(property)]
    fn active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Returns the id and placeholder values of every active request.
    fn active_requests(&self) -> Vec<(u64, Values)> {
        self.active
            .iter()
            .map(|(id, values)| (*id, values.clone()))
            .collect()
    }

    #[zbus(signal)]
    async fn request_started(
        emitter: &SignalEmitter<'_>,
        id: u64,
        values: &Values,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn request_updated(
        emitter: &SignalEmitter<'_>,
        id: u64,
        values: &Values,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn request_ended(emitter: &SignalEmitter<'_>, id: u64, reason: &str) -> zbus::Result<()>;
}

/// Connection that owns [`NAME`] and serves the request object.
pub(crate) struct Bus {
    conn: Connection,
    iface: InterfaceRef<Service>,
}

impl Bus {
    /// Connects to the session bus, serves the object and requests [`NAME`]
    /// without queueing or replacement.
    ///
    /// Fails with [`IpcError::AlreadyRunning`] for `dbus` when another
    /// connection owns the name, and with [`IpcError::TimedOut`] when the
    /// bus does not complete this within [`CONNECT_TIMEOUT`].
    #[tracing::instrument(name = "dbus_connect", skip_all)]
    pub(crate) async fn connect() -> Result<Bus, IpcError> {
        timeout(CONNECT_TIMEOUT, Self::setup())
            .await
            .map_err(|source| IpcError::TimedOut {
                context: "session bus did not answer in time",
                source,
            })?
    }

    async fn setup() -> Result<Bus, IpcError> {
        let service = Service {
            active: BTreeMap::new(),
        };
        let conn = async {
            Builder::session()?
                .serve_at(PATH, service)?
                .method_timeout(METHOD_TIMEOUT)
                .allow_name_replacements(false)
                .replace_existing_names(false)
                .name(NAME)?
                .build()
                .await
        }
        .await
        .map_err(|error| match error {
            zbus::Error::NameTaken => {
                tracing::debug!(name = NAME, "bus name owned by another instance");
                IpcError::AlreadyRunning { endpoint: "dbus" }
            }
            source => IpcError::Dbus {
                context: "cannot set up the session bus service",
                source: Box::new(source),
            },
        })?;
        let iface = conn
            .object_server()
            .interface::<_, Service>(PATH)
            .await
            .map_err(|source| IpcError::Dbus {
                context: "cannot look up the served interface",
                source: Box::new(source),
            })?;
        tracing::info!(name = NAME, path = PATH, "acquired bus name");
        Ok(Bus { conn, iface })
    }

    /// Mirrors `events` until the feed closes, then releases [`NAME`] and
    /// closes the connection. After a lag it repairs from `active`, which
    /// is updated before each event is sent. When `cancel` fires, including
    /// during a signal emission or the close, the connection is dropped
    /// without releasing the name; the bus releases it when the connection
    /// closes.
    #[tracing::instrument(name = "dbus", skip_all)]
    pub(crate) async fn run(
        self,
        mut events: broadcast::Receiver<WireEvent>,
        active: watch::Receiver<Arc<Vec<WireEvent>>>,
        cancel: CancellationToken,
    ) -> Result<(), IpcError> {
        let mirrored = cancel
            .run_until_cancelled(async {
                loop {
                    match events.recv().await {
                        Ok(ev) => self.apply(&ev).await,
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                skipped,
                                "D-Bus task fell behind, repairing from the active set"
                            );
                            // Events still queued are older than the active set read below.
                            events = events.resubscribe();
                            let snapshot = Arc::clone(&active.borrow());
                            self.replace(&snapshot).await;
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            })
            .await;
        if mirrored.is_none() {
            tracing::debug!("D-Bus task cancelled");
            return Ok(());
        }
        cancel
            .run_until_cancelled(self.close())
            .await
            .unwrap_or_else(|| {
                tracing::debug!("D-Bus task cancelled while closing");
                Ok(())
            })
    }

    /// Applies `ev` to the served set and emits its signal, unless a repair
    /// already did: a `Started` for a request served with the same values
    /// or an `Ended` for a request not served emits nothing. An `Updated`
    /// already covered by a repair is emitted again.
    #[tracing::instrument(level = "trace", skip_all, fields(id = ev.id, kind = ?ev.kind))]
    async fn apply(&self, ev: &WireEvent) {
        let (was_active, is_active, repeated) = {
            let mut service = self.iface.get_mut().await;
            let was_active = !service.active.is_empty();
            let repeated = match ev.kind {
                Kind::Started | Kind::Updated => {
                    let known = service.active.insert(ev.id, ev.values.clone());
                    ev.kind == Kind::Started && known.as_ref() == Some(&ev.values)
                }
                Kind::Ended => service.active.remove(&ev.id).is_none(),
            };
            (was_active, !service.active.is_empty(), repeated)
        };
        if repeated {
            tracing::debug!(id = ev.id, kind = ?ev.kind, "signal already emitted by a repair");
            return;
        }
        self.emit_request(
            ev.kind,
            ev.id,
            &ev.values,
            ev.reason.as_deref().unwrap_or_default(),
        )
        .await;
        if was_active != is_active {
            self.emit_active().await;
        }
    }

    /// Replaces the served set with `snapshot` and emits a signal for each
    /// difference: `RequestEnded` with an empty reason for a request no
    /// longer present, `RequestStarted` or `RequestUpdated` for a new or
    /// changed one, and `PropertiesChanged` when `Active` flips.
    #[tracing::instrument(level = "debug", skip_all, fields(active = snapshot.len()))]
    async fn replace(&self, snapshot: &[WireEvent]) {
        let fresh: BTreeMap<u64, Values> = snapshot
            .iter()
            .map(|ev| (ev.id, ev.values.clone()))
            .collect();
        let old = std::mem::replace(&mut self.iface.get_mut().await.active, fresh.clone());
        tracing::debug!(
            old = old.len(),
            new = fresh.len(),
            "D-Bus active set replaced"
        );
        for id in old.keys().filter(|id| !fresh.contains_key(id)) {
            self.emit_request(Kind::Ended, *id, &Values::new(), "")
                .await;
        }
        for (id, values) in &fresh {
            match old.get(id) {
                None => self.emit_request(Kind::Started, *id, values, "").await,
                Some(known) if known != values => {
                    self.emit_request(Kind::Updated, *id, values, "").await;
                }
                Some(_) => {}
            }
        }
        if old.is_empty() != fresh.is_empty() {
            self.emit_active().await;
        }
    }

    async fn emit_request(&self, kind: Kind, id: u64, values: &Values, reason: &str) {
        let emitter = self.iface.signal_emitter();
        let sent = match kind {
            Kind::Started => Service::request_started(emitter, id, values).await,
            Kind::Updated => Service::request_updated(emitter, id, values).await,
            Kind::Ended => Service::request_ended(emitter, id, reason).await,
        };
        if let Err(error) = sent {
            tracing::warn!(
                id,
                ?kind,
                path = PATH,
                error = &error as &dyn Error,
                "cannot emit request signal"
            );
        }
    }

    async fn emit_active(&self) {
        let sent = self
            .iface
            .get()
            .await
            .active_changed(self.iface.signal_emitter())
            .await;
        if let Err(error) = sent {
            tracing::warn!(
                path = PATH,
                error = &error as &dyn Error,
                "cannot emit PropertiesChanged"
            );
        }
    }

    /// Releases [`NAME`] and closes the connection, logging each failure
    /// and returning the first.
    ///
    /// Fails with [`IpcError::TimedOut`] when this takes longer than
    /// [`CLOSE_TIMEOUT`]; the connection is then dropped, which also
    /// releases the name.
    #[tracing::instrument(name = "dbus_close", skip_all)]
    pub(crate) async fn close(self) -> Result<(), IpcError> {
        match timeout(CLOSE_TIMEOUT, self.release()).await {
            Ok(released) => released,
            Err(source) => {
                tracing::warn!(
                    timeout_ms = CLOSE_TIMEOUT.as_millis(),
                    "D-Bus close timed out, connection dropped"
                );
                Err(IpcError::TimedOut {
                    context: "session bus did not close in time",
                    source,
                })
            }
        }
    }

    async fn release(self) -> Result<(), IpcError> {
        let Bus { conn, iface } = self;
        drop(iface);
        let released = match conn.release_name(NAME).await {
            Ok(owned) => {
                tracing::info!(name = NAME, owned, "released bus name");
                Ok(())
            }
            Err(source) => {
                tracing::warn!(
                    name = NAME,
                    error = &source as &dyn Error,
                    "cannot release bus name"
                );
                Err(IpcError::Dbus {
                    context: "cannot release the bus name",
                    source: Box::new(source),
                })
            }
        };
        let closed = conn.close().await.map_err(|source| {
            tracing::warn!(error = &source as &dyn Error, "cannot close bus connection");
            IpcError::Dbus {
                context: "cannot close the bus connection",
                source: Box::new(source),
            }
        });
        released.and(closed)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::time::{Instant, sleep};
    use tokio_util::sync::CancellationToken;
    use touchcue_core::{Event, RequestState, Source};
    use zbus::Proxy;

    use super::*;
    use crate::tests::TestError;
    use crate::wire::tests::request;
    use crate::{Ipc, IpcConfig};

    const IFACE: &str = "io.github.theaifam5.Touchcue1";
    /// Deadline for the service to reflect a published event.
    const DEADLINE: Duration = Duration::from_secs(30);
    /// Interval between two polls of the service.
    const POLL: Duration = Duration::from_millis(20);

    type Requests = Vec<(u64, Values)>;

    #[tokio::test]
    #[ignore = "needs a session bus"]
    async fn serves_requests_and_owns_the_name() -> Result<(), TestError> {
        let cfg = || IpcConfig {
            runtime_dir: PathBuf::from("/nonexistent"),
            json: false,
            dbus: true,
            compat_maxbaz: false,
        };
        let ipc = Ipc::spawn(cfg(), CancellationToken::new()).await?;
        assert!(ipc.endpoints().dbus);
        // A taken name means another touchcue runs, which fails the spawn.
        assert!(matches!(
            Ipc::spawn(cfg(), CancellationToken::new()).await,
            Err(IpcError::AlreadyRunning { endpoint: "dbus" })
        ));

        let client = Connection::session().await?;
        let proxy = Proxy::new(&client, NAME, PATH, IFACE).await?;
        assert!(!proxy.get_property::<bool>("Active").await?);
        let xml = proxy.introspect().await?;
        for signal in ["RequestStarted", "RequestUpdated", "RequestEnded"] {
            assert!(
                xml.contains(&format!("<signal name=\"{signal}\">")),
                "{signal}"
            );
        }

        let values = BTreeMap::from([("app.name".to_owned(), "ssh".to_owned())]);
        let event = Event::Started(request(5, Source::Fido, RequestState::Waiting));
        ipc.publish(&WireEvent::new(&event, &values));
        let deadline = Instant::now() + DEADLINE;
        let mut requests: Requests = proxy.call("ActiveRequests", &()).await?;
        while requests.is_empty() && Instant::now() < deadline {
            sleep(POLL).await;
            requests = proxy.call("ActiveRequests", &()).await?;
        }
        assert_eq!(requests, vec![(5, values)]);
        // The proxy caches `Active` and only updates it from `PropertiesChanged`.
        let mut active = proxy.get_property::<bool>("Active").await?;
        while !active && Instant::now() < deadline {
            sleep(POLL).await;
            active = proxy.get_property::<bool>("Active").await?;
        }
        assert!(active);

        ipc.shutdown().await?;
        let again = Ipc::spawn(cfg(), CancellationToken::new()).await?;
        again.shutdown().await?;
        Ok(())
    }
}
