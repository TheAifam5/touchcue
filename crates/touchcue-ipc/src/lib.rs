//! JSON event socket, compatibility socket, D-Bus service and helper protocol.
//!
//! On Linux, [`Ipc::spawn`] serves:
//!
//! - `<runtime_dir>/touchcue/events.sock`: one JSON [`WireEvent`] per line,
//!   starting with a `started` event per active request;
//! - `<runtime_dir>/yubikey-touch-detector.socket`: the 5-byte `U2F_1`,
//!   `GPG_0`, `MAC_1` style messages of maximbaz/yubikey-touch-detector,
//!   starting with the `_1` message of every group with a request waiting
//!   for a touch;
//! - the session bus name `io.github.theaifam5.Touchcue`.
//!
//! A live JSON socket or an owned bus name means another touchcue runs, and
//! [`Ipc::spawn`] fails with [`IpcError::AlreadyRunning`]. Any other endpoint
//! that cannot start, including a compat socket served by
//! maximbaz/yubikey-touch-detector, is skipped with a warning. The compat
//! socket starts only when `runtime_dir` is a private directory owned by
//! this user.
//!
//! Clients only receive; a client that closes its write side is treated as
//! gone and disconnected. A client whose peer uid differs from the
//! daemon's effective uid is closed at once.
//!
//! Events carry only allowlisted placeholder values: `request.*`,
//! `device.{vendor,model,product,kind,transport,vid,pid}`,
//! `app.{name,id,icon,container}`, `process.{name,chain}` and
//! `requester.{name,label}`. The icon path in
//! `app.icon` is the only path published; executable paths, uids, pids and
//! command lines never leave the daemon.
//!
//! The endpoints run as tasks on the caller's Tokio runtime, which needs
//! the I/O and time drivers enabled.
//!
//! On other platforms [`Ipc`] serves nothing.

#[cfg(target_os = "linux")]
pub mod agent;
#[cfg(target_os = "linux")]
mod client;
#[cfg(target_os = "linux")]
mod dbus;
#[cfg(target_os = "linux")]
mod dispatch;
#[cfg(target_os = "linux")]
pub mod helper;
#[cfg(target_os = "linux")]
mod limit;
#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod sockdiag;
#[cfg(target_os = "linux")]
mod socket;
mod wire;

use std::path::PathBuf;

pub use wire::{Kind, WireEvent};

/// Endpoints to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcConfig {
    /// Directory for the sockets, normally `$XDG_RUNTIME_DIR`.
    pub runtime_dir: PathBuf,
    /// Serve `<runtime_dir>/touchcue/events.sock`.
    pub json: bool,
    /// Serve the session bus service.
    pub dbus: bool,
    /// Serve `<runtime_dir>/yubikey-touch-detector.socket`.
    pub compat_maxbaz: bool,
}

/// Endpoints that started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Endpoints {
    pub json: bool,
    pub compat: bool,
    pub dbus: bool,
}

/// Failure to start or stop the IPC endpoints.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// I/O on a socket file or its directory failed.
    #[error("{context}: {}", path.display())]
    Socket {
        context: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A socket path or its directory could be reached or replaced by another user.
    #[error("{context}: {}", path.display())]
    Insecure {
        context: &'static str,
        path: PathBuf,
    },
    /// Another instance serves `endpoint` (`json`, `compat` or `dbus`).
    #[error("another instance serves the {endpoint} endpoint")]
    AlreadyRunning { endpoint: &'static str },
    #[cfg(target_os = "linux")]
    #[error("{context}")]
    Dbus {
        context: &'static str,
        #[source]
        source: Box<zbus::Error>,
    },
    /// An IPC task panicked or was aborted.
    #[cfg(target_os = "linux")]
    #[error("IPC task {task} failed")]
    TaskFailed {
        task: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    /// An operation did not finish within its deadline.
    #[cfg(target_os = "linux")]
    #[error("{context}")]
    TimedOut {
        context: &'static str,
        #[source]
        source: tokio::time::error::Elapsed,
    },
    /// Tasks still ran after the shutdown drain and cancellation deadlines.
    #[cfg(target_os = "linux")]
    #[error("IPC tasks did not stop in time")]
    ShutdownTimedOut(#[source] tokio::time::error::Elapsed),
}

#[cfg(target_os = "linux")]
pub use linux::Ipc;

#[cfg(target_os = "linux")]
mod linux {
    use std::error::Error;
    use std::os::unix::net::UnixListener as StdListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::net::UnixListener;
    use tokio::sync::mpsc::error::TrySendError;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use tokio_util::task::TaskTracker;
    use tracing::Instrument;

    use crate::client;
    use crate::dbus::Bus;
    use crate::dispatch::{BusFeed, Dispatcher, Endpoint, Msg};
    use crate::limit::LogLimit;
    use crate::socket::{self, SocketFile};
    use crate::{Endpoints, IpcConfig, IpcError, WireEvent};

    /// Events and resyncs queued for the dispatcher before
    /// [`Ipc::publish`] and [`Ipc::resync`] drop them.
    const EVENT_QUEUE: usize = 256;
    /// Accepted clients queued for registration before further ones are refused.
    const REGISTRATION_QUEUE: usize = 32;
    /// Events the D-Bus task may fall behind before it repairs from the active set.
    const BUS_BUFFER: usize = 64;
    /// Time [`Ipc::shutdown`] lets the tasks deliver queued events.
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
    /// Time [`Ipc::shutdown`] waits for cancelled tasks after the drain.
    const CANCEL_TIMEOUT: Duration = Duration::from_millis(500);

    /// Running IPC endpoints.
    ///
    /// Dropping it without [`Ipc::shutdown`] cancels every task without
    /// waiting and removes the socket files.
    #[derive(Debug)]
    pub struct Ipc {
        events: Option<mpsc::Sender<Msg>>,
        /// Child of the caller's token; cancels every task.
        cancel: CancellationToken,
        /// Child of `cancel`; stops the accept loops.
        accepting: CancellationToken,
        tracker: TaskTracker,
        dispatcher: Option<JoinHandle<()>>,
        bus: Option<JoinHandle<Result<(), IpcError>>>,
        sockets: Vec<SocketFile>,
        endpoints: Endpoints,
        drop_limit: LogLimit,
    }

    /// Keeps the first failure and logs every one.
    fn failed(first: &mut Option<IpcError>, endpoint: &'static str, error: IpcError) {
        tracing::warn!(
            endpoint,
            error = &error as &dyn Error,
            "IPC endpoint not started"
        );
        first.get_or_insert(error);
    }

    async fn json_socket(runtime_dir: &Path) -> Result<(StdListener, SocketFile), IpcError> {
        let dir = runtime_dir.join("touchcue");
        let path = dir.join("events.sock");
        socket::blocking(move || socket::private_dir(&dir)).await?;
        socket::bind(path, "json").await
    }

    async fn compat_socket(runtime_dir: &Path) -> Result<(StdListener, SocketFile), IpcError> {
        let dir = runtime_dir.to_owned();
        socket::blocking(move || socket::check_private_dir(&dir)).await?;
        socket::bind(runtime_dir.join("yubikey-touch-detector.socket"), "compat").await
    }

    impl Ipc {
        /// Starts every requested endpoint that can start and serves them
        /// until [`Ipc::shutdown`], drop, or `cancel` fires.
        ///
        /// A live JSON socket or an owned bus name means another touchcue
        /// runs: the endpoints already started are closed and the spawn
        /// fails. Any other endpoint that cannot start, such as a compat
        /// socket served by another program, a missing session bus, or a
        /// `runtime_dir` that is not private for the compat socket, is logged
        /// at `warn` and skipped; [`Ipc::endpoints`] reports what runs.
        ///
        /// An existing socket that accepts, has a full backlog, or does not
        /// answer within 500 ms counts as live. Connecting to the session
        /// bus and acquiring the name get 5 s; a bus that does not answer
        /// in time is skipped like a missing one.
        ///
        /// Must be called within a Tokio runtime with the I/O and time drivers.
        ///
        /// # Errors
        ///
        /// Returns [`IpcError::AlreadyRunning`] for the `json` or `dbus`
        /// endpoint when another touchcue runs. Otherwise returns the first
        /// endpoint failure when endpoints were requested and none started:
        /// [`IpcError::Insecure`], [`IpcError::AlreadyRunning`] for `compat`,
        /// [`IpcError::Dbus`], [`IpcError::TimedOut`] or [`IpcError::Socket`].
        /// Returns [`IpcError::TaskFailed`] when the blocking socket setup
        /// task fails.
        #[tracing::instrument(name = "ipc_spawn", skip_all, fields(runtime_dir = %cfg.runtime_dir.display()))]
        pub async fn spawn(cfg: IpcConfig, cancel: CancellationToken) -> Result<Ipc, IpcError> {
            let IpcConfig {
                runtime_dir,
                json,
                dbus,
                compat_maxbaz,
            } = cfg;
            tracing::debug!(json, dbus, compat_maxbaz, "starting IPC");
            let mut first = None;
            let mut endpoints = Endpoints::default();
            let bus = if dbus {
                match Bus::connect().await {
                    Ok(bus) => {
                        endpoints.dbus = true;
                        Some(bus)
                    }
                    Err(error @ IpcError::AlreadyRunning { .. }) => return Err(error),
                    Err(error) => {
                        failed(&mut first, "dbus", error);
                        None
                    }
                }
            } else {
                None
            };
            let mut listeners = Vec::new();
            let mut sockets = Vec::new();
            let uid = socket::euid();
            if json {
                match json_socket(&runtime_dir).await {
                    Ok((listener, file)) => {
                        endpoints.json = true;
                        listeners.push((Endpoint::Json, listener, file.path().to_owned(), uid));
                        sockets.push(file);
                    }
                    Err(error @ IpcError::AlreadyRunning { .. }) => {
                        if let Some(bus) = bus
                            && let Err(close) = bus.close().await
                        {
                            tracing::warn!(
                                error = &close as &dyn Error,
                                "cannot close D-Bus after a failed start"
                            );
                        }
                        return Err(error);
                    }
                    Err(error) => failed(&mut first, "json", error),
                }
            }
            if compat_maxbaz {
                match compat_socket(&runtime_dir).await {
                    Ok((listener, file)) => {
                        endpoints.compat = true;
                        listeners.push((Endpoint::Compat, listener, file.path().to_owned(), uid));
                        sockets.push(file);
                    }
                    Err(error) => failed(&mut first, "compat", error),
                }
            }
            if let Some(error) = first.filter(|_| endpoints == Endpoints::default()) {
                return Err(error);
            }
            let cancel = cancel.child_token();
            let accepting = cancel.child_token();
            let mut ipc = Ipc {
                events: None,
                cancel,
                accepting,
                tracker: TaskTracker::new(),
                dispatcher: None,
                bus: None,
                sockets,
                endpoints,
                drop_limit: LogLimit::new(),
            };
            ipc.start(bus, listeners)?;
            Ok(ipc)
        }

        /// Starts the D-Bus, dispatcher and accept tasks.
        fn start(
            &mut self,
            bus: Option<Bus>,
            listeners: Vec<(Endpoint, StdListener, PathBuf, u32)>,
        ) -> Result<(), IpcError> {
            let feed = bus.map(|bus| {
                let (events, events_rx) = broadcast::channel(BUS_BUFFER);
                let (active, active_rx) = watch::channel(Arc::new(Vec::new()));
                let task = bus.run(events_rx, active_rx, self.cancel.clone());
                self.bus = Some(self.tracker.spawn(task));
                BusFeed { events, active }
            });
            let (events, events_rx) = mpsc::channel(EVENT_QUEUE);
            let (registrations, registrations_rx) = mpsc::channel(REGISTRATION_QUEUE);
            self.events = Some(events);
            let dispatcher =
                Dispatcher::new(feed).run(events_rx, registrations_rx, self.cancel.clone());
            let span = tracing::info_span!("ipc_dispatcher");
            self.dispatcher = Some(self.tracker.spawn(dispatcher.instrument(span)));
            for (endpoint, listener, path, uid) in listeners {
                let listener =
                    UnixListener::from_std(listener).map_err(|source| IpcError::Socket {
                        context: "cannot register socket with the runtime",
                        path: path.clone(),
                        source,
                    })?;
                tracing::info!(?endpoint, path = %path.display(), "serving IPC socket");
                let span = tracing::info_span!("ipc_accept", ?endpoint, path = %path.display());
                let task = client::accept_loop(
                    listener,
                    endpoint,
                    uid,
                    registrations.clone(),
                    self.tracker.clone(),
                    self.accepting.clone(),
                    self.cancel.clone(),
                );
                self.tracker.spawn(task.instrument(span));
            }
            Ok(())
        }

        /// Returns the endpoints that started.
        #[must_use]
        pub fn endpoints(&self) -> Endpoints {
            self.endpoints
        }

        /// Queues `ev` for every client without blocking; when the queue is
        /// full the event is dropped and logged at most once per second.
        pub fn publish(&self, ev: &WireEvent) {
            self.send(Msg::Event(ev.clone()), ev.id);
        }

        /// Replaces the active set with `active`, the daemon's authoritative
        /// list of active requests, without blocking.
        ///
        /// Clients receive an `ended` event for each request no longer
        /// active, carrying `reason: null` and the request's last state and
        /// values, a `started` or `updated` event for each new or
        /// changed one, and the compat and `Active` changes that follow. The
        /// `kind` and `reason` of the given events are ignored. When the
        /// queue is full the resync is dropped and logged like an event.
        pub fn resync(&self, active: &[WireEvent]) {
            self.send(Msg::Resync(active.to_vec()), 0);
        }

        fn send(&self, msg: Msg, id: u64) {
            let Some(events) = &self.events else {
                return;
            };
            let reason = match events.try_send(msg) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => "IPC queue full",
                Err(TrySendError::Closed(_)) => "IPC dispatcher stopped",
            };
            if let Some(suppressed) = self.drop_limit.allow() {
                tracing::warn!(id, reason, suppressed, "IPC event dropped");
            }
        }

        /// Stops accepting, removes the socket files, delivers the queued
        /// events, closes every client and releases the bus name.
        ///
        /// Delivery gets [`DRAIN_TIMEOUT`]; tasks still running then are
        /// cancelled and get [`CANCEL_TIMEOUT`] more, so this returns within
        /// about 2.5 s.
        ///
        /// # Errors
        ///
        /// Returns the first failure after attempting every step:
        /// [`IpcError::Socket`] when a socket file cannot be removed,
        /// [`IpcError::Dbus`] when the name cannot be released,
        /// [`IpcError::TaskFailed`] when a task panicked, or
        /// [`IpcError::ShutdownTimedOut`].
        #[tracing::instrument(name = "ipc_shutdown", skip_all)]
        pub async fn shutdown(mut self) -> Result<(), IpcError> {
            // Every failure is logged here because only the first one is returned.
            let mut result = Ok(());
            for file in &mut self.sockets {
                if let Err(error) = file.remove() {
                    tracing::warn!(error = &error as &dyn Error, "cannot remove socket file");
                    result = result.and(Err(error));
                }
            }
            self.accepting.cancel();
            self.events = None;
            self.tracker.close();
            let dispatcher = self.dispatcher.take();
            let bus = self.bus.take();
            let tracker = self.tracker.clone();
            let drained = timeout(DRAIN_TIMEOUT, async {
                let mut drained = Ok(());
                if let Some(task) = dispatcher {
                    drained = drained.and(joined("dispatcher", task.await.map(Ok)));
                }
                if let Some(task) = bus {
                    drained = drained.and(joined("D-Bus", task.await));
                }
                tracker.wait().await;
                drained
            })
            .await;
            match drained {
                Ok(drained) => result = result.and(drained),
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_ms = DRAIN_TIMEOUT.as_millis(),
                        "IPC drain timed out, cancelling"
                    );
                    self.cancel.cancel();
                    match timeout(CANCEL_TIMEOUT, self.tracker.wait()).await {
                        Ok(()) => {}
                        Err(elapsed) => {
                            tracing::warn!(
                                timeout_ms = CANCEL_TIMEOUT.as_millis(),
                                "IPC tasks still running after cancel"
                            );
                            result = result.and(Err(IpcError::ShutdownTimedOut(elapsed)));
                        }
                    }
                }
            }
            if let Ok(()) = &result {
                tracing::info!("IPC stopped");
            }
            result
        }
    }

    /// Flattens a task outcome, logging a failure.
    fn joined(
        task: &'static str,
        outcome: Result<Result<(), IpcError>, tokio::task::JoinError>,
    ) -> Result<(), IpcError> {
        let ended = outcome
            .map_err(|source| IpcError::TaskFailed { task, source })
            .and_then(|ended| ended);
        if let Err(error) = &ended {
            tracing::warn!(
                task,
                error = error as &dyn Error,
                "IPC task ended with an error"
            );
        }
        ended
    }

    impl Drop for Ipc {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use tokio_util::sync::CancellationToken;

    use crate::{Endpoints, IpcConfig, IpcError, WireEvent};

    /// IPC endpoints, none of which exist on this platform.
    #[derive(Debug)]
    pub struct Ipc;

    impl Ipc {
        /// Returns an instance that serves nothing.
        ///
        /// # Errors
        ///
        /// Never fails on this platform.
        pub fn spawn(
            _cfg: IpcConfig,
            _cancel: CancellationToken,
        ) -> impl Future<Output = Result<Ipc, IpcError>> {
            std::future::ready(Ok(Ipc))
        }

        /// Returns that no endpoint runs.
        #[must_use]
        pub fn endpoints(&self) -> Endpoints {
            Endpoints::default()
        }

        /// Does nothing on this platform.
        pub fn publish(&self, _ev: &WireEvent) {}

        /// Does nothing on this platform.
        pub fn resync(&self, _active: &[WireEvent]) {}

        /// Does nothing on this platform.
        ///
        /// # Errors
        ///
        /// Never fails on this platform.
        pub fn shutdown(self) -> impl Future<Output = Result<(), IpcError>> {
            std::future::ready(Ok(()))
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use other::Ipc;

#[cfg(all(test, target_os = "linux"))]
mod tests;
