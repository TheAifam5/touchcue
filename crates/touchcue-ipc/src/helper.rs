//! The helper socket, `<runtime_dir>/touchcue/helper.sock`, through which
//! `touchcue scdaemon` and `touchcue askpass` report operations, and their
//! translation into `OpenPGP` signals and ssh notices.
//!
//! Each connection sends [`touchcue_core::helper`] lines and never gets a
//! reply. An operation is open from its `start` line to its `end` line;
//! one still open when its connection closes ends as failed, because its
//! reporter died.
//!
//! An scdaemon operation becomes a pending signal only when the user
//! interaction flag (UIF) of its card key slot requires a touch, or the
//! flag is unknown. The flags are read from gpg-agent only while no
//! operation is open, because gpg-agent serializes card access and a read
//! during a touch wait would block behind it: after the first scdaemon
//! report, after a new scdaemon connection at most every 30 s, and after
//! [`UIF_TTL`] once an scdaemon operation started since the last read. Until
//! the first read, every operation is reported.
//!
//! An askpass operation never becomes a request of its own: the FIDO
//! source already detects the touch it announces. It becomes a [`Notice`]
//! that lets the daemon add the operation's detail to that FIDO request.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, sleep, timeout};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use touchcue_core::assuan;
use touchcue_core::helper::{self, Message, Origin};
use touchcue_core::{
    Device, DeviceId, DeviceKind, Method, Op, Outcome, RateLimit, Signal, SignalClass, SignalKind,
    Source, Transport,
};
use tracing::Instrument;

use crate::IpcError;
use crate::agent::{self, AgentError, AgentPaths, CardState};
use crate::sockdiag;
use crate::socket::{self, SocketFile};

/// Most reporter connections served at once; further ones are closed at once.
pub const MAX_CLIENTS: usize = 16;
/// Most operations one connection may hold open.
const MAX_OPEN_OPS: usize = 16;
/// Time a connection may stay silent before it is closed, so idle
/// connections cannot hold every slot. The scdaemon wrapper connects lazily
/// and reconnects after a failed write.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Most starts one connection may send per [`START_WINDOW`]; further starts are dropped.
const MAX_STARTS: u32 = 20;
/// Window of [`MAX_STARTS`].
const START_WINDOW: Duration = Duration::from_secs(10);
/// Reports queued between the connections and the translating task.
const REPORT_QUEUE: usize = 64;
/// Longest line read from a reporter, in bytes; [`helper::MAX_LINE`].
const LINE_READ: u64 = helper::MAX_LINE as u64;
/// Largest `stat` file read for a parent pid, in bytes.
const STAT_MAX: u64 = 4096;
/// Pause after a failed accept, such as when the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Shortest interval between two logged warnings of one kind.
const WARN_INTERVAL: Duration = Duration::from_secs(10);
/// Longest time an operation is kept waiting; a reporter that neither
/// ends it nor disconnects by then is treated as hung.
const MAX_OP_AGE: Duration = Duration::from_secs(120);
/// Shortest time between a UIF read and one caused by a new scdaemon connection.
const MIN_READ_INTERVAL: Duration = Duration::from_secs(30);
/// Age after which the cached UIF values are read again.
pub const UIF_TTL: Duration = Duration::from_secs(600);
/// Longest wait for the `sock_diag` check of the ssh socket.
const SSH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);
/// Time [`Helper::stop`] waits for its tasks after cancelling them.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Device id of every `OpenPGP` card; the card serial number is never used.
pub const OPENPGP_DEVICE: &str = "openpgp";

/// Settings of the helper socket.
#[derive(Debug, Clone)]
pub struct HelperConfig {
    /// Directory for the socket, normally `$XDG_RUNTIME_DIR`.
    pub runtime_dir: PathBuf,
    /// gpg-agent's sockets; without them every scdaemon operation is
    /// reported and `PKAUTH` counts as gpg.
    pub agent: Option<AgentPaths>,
    /// Interval of the progress signals that keep the request of an open
    /// operation waiting; must be below the machine's keepalive timeout.
    pub keepalive: Duration,
}

/// An ssh user-presence notice reported by `touchcue askpass`.
///
/// `pid` is the program that started askpass, ssh or ssh-agent.
#[derive(Clone, PartialEq, Eq)]
pub enum Notice {
    /// `pid` waits for a touch for the operation `detail` describes.
    /// `detail` is untrusted text, such as a key fingerprint and
    /// destination, at most [`helper::MAX_DETAIL`] bytes; never log it.
    Started { pid: u32, detail: String },
    /// The notice of `pid` ended.
    Ended { pid: u32 },
}

impl fmt::Debug for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Started { pid, .. } => f
                .debug_struct("Started")
                .field("pid", pid)
                .finish_non_exhaustive(),
            Self::Ended { pid } => f.debug_struct("Ended").field("pid", pid).finish(),
        }
    }
}

/// Destinations of the helper's output.
#[derive(Debug, Clone)]
pub struct HelperOutputs {
    pub signals: mpsc::Sender<Signal>,
    pub notices: mpsc::Sender<Notice>,
}

/// The running helper socket.
///
/// Dropping it cancels its tasks without waiting and removes the socket file.
#[derive(Debug)]
pub struct Helper {
    cancel: CancellationToken,
    tracker: TaskTracker,
    translator: JoinHandle<()>,
    socket: SocketFile,
}

impl Helper {
    /// Binds the helper socket and serves it until [`Helper::stop`], drop,
    /// or `parent` is cancelled, sending the resulting signals and notices
    /// to `outputs`.
    ///
    /// The socket is created with mode 0600 in `<runtime_dir>/touchcue`,
    /// a 0700 directory owned by this user. Clients of another user are
    /// closed at once. Must be called within a Tokio runtime with the I/O
    /// and time drivers.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::Insecure`] when the directory or an existing
    /// socket file is unsafe, [`IpcError::AlreadyRunning`] when another
    /// process serves the socket, [`IpcError::Socket`] when it cannot be
    /// bound, or [`IpcError::TaskFailed`] when the setup task fails.
    #[tracing::instrument(name = "helper_spawn", skip_all, fields(path = tracing::field::Empty), err)]
    pub async fn spawn(
        cfg: HelperConfig,
        outputs: HelperOutputs,
        parent: &CancellationToken,
    ) -> Result<Self, IpcError> {
        let dir = cfg.runtime_dir.join("touchcue");
        let path = dir.join("helper.sock");
        tracing::Span::current().record("path", tracing::field::display(path.display()));
        socket::blocking(move || socket::private_dir(&dir)).await?;
        let (listener, file) = socket::bind(path, "helper").await?;
        let listener = UnixListener::from_std(listener).map_err(|source| IpcError::Socket {
            context: "cannot register socket with the runtime",
            path: file.path().to_owned(),
            source,
        })?;
        let cancel = parent.child_token();
        let tracker = TaskTracker::new();
        let (reports, reports_rx) = mpsc::channel(REPORT_QUEUE);
        let translator = Translator::new(cfg.agent, outputs, tracker.clone(), cancel.clone());
        let translator = tokio::spawn(
            translator
                .run(reports_rx, cfg.keepalive)
                .instrument(tracing::info_span!("helper")),
        );
        let accept = accept_loop(
            listener,
            socket::euid(),
            reports,
            tracker.clone(),
            cancel.clone(),
        );
        let span = tracing::info_span!("helper_accept", path = %file.path().display());
        tracker.spawn(accept.instrument(span));
        tracing::info!(path = %file.path().display(), "serving helper socket");
        Ok(Self {
            cancel,
            tracker,
            translator,
            socket: file,
        })
    }

    /// Stops accepting, closes every connection, removes the socket file
    /// and waits up to 2 s for the tasks to end.
    ///
    /// # Errors
    ///
    /// Returns the first failure: [`IpcError::Socket`] when the socket file
    /// cannot be removed, [`IpcError::TaskFailed`] when the translating task
    /// panicked, or [`IpcError::ShutdownTimedOut`].
    #[tracing::instrument(name = "helper_stop", skip_all, err)]
    pub async fn stop(mut self) -> Result<(), IpcError> {
        self.cancel.cancel();
        let mut result = self.socket.remove();
        self.tracker.close();
        let tracker = self.tracker.clone();
        let translator = &mut self.translator;
        let stopped = timeout(STOP_TIMEOUT, async {
            let joined = translator.await;
            tracker.wait().await;
            joined
        })
        .await;
        match stopped {
            Ok(Ok(())) => {}
            Ok(Err(source)) => {
                result = result.and(Err(IpcError::TaskFailed {
                    task: "helper",
                    source,
                }));
            }
            Err(elapsed) => result = result.and(Err(IpcError::ShutdownTimedOut(elapsed))),
        }
        result
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A line from one connection.
struct Report {
    conn: u64,
    /// Process id of the reporter at connect time.
    pid: Option<u32>,
    message: Message,
}

/// Reports whether a peer with uid `peer`, `None` when unreadable, may
/// connect to a daemon running as `own`.
fn admitted(peer: Option<u32>, own: u32) -> bool {
    peer == Some(own)
}

/// Rate-limited warnings shared by the connection tasks.
type SharedLimit = Arc<Mutex<RateLimit>>;

fn warn_limited(limit: &SharedLimit, log: impl FnOnce(u64)) {
    let now = Instant::now().into_std();
    // A panic while logging leaves the limiter itself consistent.
    let mut limit = match limit.lock() {
        Ok(limit) => limit,
        Err(poisoned) => poisoned.into_inner(),
    };
    limit.log(now, log);
}

/// Accepts clients until `cancel` fires and starts a task per client in `tracker`.
async fn accept_loop(
    listener: UnixListener,
    uid: u32,
    reports: mpsc::Sender<Report>,
    tracker: TaskTracker,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let mut error_limit = RateLimit::new(WARN_INTERVAL);
    let mut refuse_limit = RateLimit::new(WARN_INTERVAL);
    let malformed: SharedLimit = Arc::new(Mutex::new(RateLimit::new(WARN_INTERVAL)));
    let mut next_conn: u64 = 0;
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                error_limit.log(Instant::now().into_std(), |suppressed| {
                    tracing::warn!(
                        error = &error as &dyn Error,
                        suppressed,
                        "cannot accept reporter"
                    );
                });
                tokio::select! {
                    () = cancel.cancelled() => break,
                    () = sleep(ACCEPT_BACKOFF) => continue,
                }
            }
        };
        let cred = match stream.peer_cred() {
            Ok(cred) => Some(cred),
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn Error,
                    "cannot read reporter credentials"
                );
                None
            }
        };
        if !admitted(cred.map(|c| c.uid()), uid) {
            refuse_limit.log(Instant::now().into_std(), |suppressed| {
                tracing::warn!(suppressed, "reporter of another user refused");
            });
            continue;
        }
        let permit = match Arc::clone(&slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                refuse_limit.log(Instant::now().into_std(), |suppressed| {
                    tracing::warn!(
                        limit = MAX_CLIENTS,
                        suppressed,
                        "reporter limit reached, reporter refused"
                    );
                });
                continue;
            }
            Err(error @ TryAcquireError::Closed) => {
                tracing::error!(
                    error = &error as &dyn Error,
                    "reporter slots closed, helper socket stops accepting"
                );
                break;
            }
        };
        let pid = cred
            .and_then(|c| c.pid())
            .and_then(|pid| match u32::try_from(pid) {
                Ok(pid) => Some(pid),
                Err(error) => {
                    tracing::debug!(error = &error as &dyn Error, "reporter pid out of range");
                    None
                }
            });
        next_conn = next_conn.wrapping_add(1);
        let conn = Connection {
            conn: next_conn,
            pid,
            reports: reports.clone(),
            malformed: Arc::clone(&malformed),
            open: Vec::new(),
            window: Instant::now(),
            starts: 0,
        };
        tracing::debug!(conn = next_conn, "reporter connected");
        let span = tracing::debug_span!("helper_client", conn = next_conn);
        tracker.spawn(conn.serve(stream, permit, cancel.clone()).instrument(span));
    }
    tracing::debug!("helper accept loop stopped");
}

/// One reporter connection.
struct Connection {
    conn: u64,
    pid: Option<u32>,
    reports: mpsc::Sender<Report>,
    malformed: SharedLimit,
    /// Operations started and not ended, in start order.
    open: Vec<(Origin, u32)>,
    /// Start of the current [`START_WINDOW`] and the starts accepted in it.
    window: Instant,
    starts: u32,
}

impl Connection {
    /// Forwards each valid line until the client hangs up, sends an
    /// oversized line, stays silent for [`IDLE_TIMEOUT`], or `cancel` fires;
    /// then ends the operations still open as failed, unless cancelled.
    async fn serve(
        mut self,
        stream: UnixStream,
        _slot: OwnedSemaphorePermit,
        cancel: CancellationToken,
    ) {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut limited = (&mut reader).take(LINE_READ);
            let read = tokio::select! {
                () = cancel.cancelled() => return,
                read = timeout(IDLE_TIMEOUT, limited.read_until(b'\n', &mut line)) => read,
            };
            let read = match read {
                Ok(read) => read,
                Err(_elapsed) => {
                    tracing::debug!("reporter idle, closed");
                    break;
                }
            };
            match read {
                Ok(0) => break,
                Ok(_) if line.last() != Some(&b'\n') => {
                    if line.len() >= helper::MAX_LINE {
                        warn_limited(&self.malformed, |suppressed| {
                            tracing::warn!(suppressed, "reporter line too long, reporter closed");
                        });
                    }
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(error = &error as &dyn Error, "reporter read failed");
                    break;
                }
            }
            let parsed = match std::str::from_utf8(&line) {
                Ok(text) => Message::parse(text),
                Err(error) => {
                    tracing::trace!(error = &error as &dyn Error, "reporter line is not UTF-8");
                    Err(helper::ParseError::Malformed)
                }
            };
            let message = match parsed {
                Ok(message) => message,
                Err(error) => {
                    warn_limited(&self.malformed, |suppressed| {
                        tracing::warn!(
                            error = &error as &dyn Error,
                            suppressed,
                            "malformed reporter line dropped"
                        );
                    });
                    continue;
                }
            };
            if !self.track(&message) {
                continue;
            }
            if !self.forward(message, &cancel).await {
                return;
            }
        }
        tracing::debug!(open = self.open.len(), "reporter disconnected");
        for (origin, seq) in std::mem::take(&mut self.open) {
            let message = Message::End {
                origin,
                seq,
                outcome: Outcome::Failed,
            };
            if !self.forward(message, &cancel).await {
                return;
            }
        }
    }

    /// Updates the open operations and reports whether `message` applies.
    fn track(&mut self, message: &Message) -> bool {
        match *message {
            Message::Start { origin, seq, .. } => {
                if self.open.contains(&(origin, seq)) {
                    tracing::debug!(seq, "repeated start ignored");
                    return false;
                }
                let now = Instant::now();
                if now.saturating_duration_since(self.window) >= START_WINDOW {
                    self.window = now;
                    self.starts = 0;
                }
                if self.starts >= MAX_STARTS {
                    warn_limited(&self.malformed, |suppressed| {
                        tracing::warn!(
                            limit = MAX_STARTS,
                            suppressed,
                            "reporter sends starts too fast, start dropped"
                        );
                    });
                    return false;
                }
                self.starts = self.starts.saturating_add(1);
                if self.open.len() >= MAX_OPEN_OPS {
                    warn_limited(&self.malformed, |suppressed| {
                        tracing::warn!(
                            limit = MAX_OPEN_OPS,
                            suppressed,
                            "too many open operations, start dropped"
                        );
                    });
                    return false;
                }
                self.open.push((origin, seq));
                true
            }
            Message::End { origin, seq, .. } => {
                let before = self.open.len();
                self.open.retain(|&op| op != (origin, seq));
                self.open.len() < before
            }
        }
    }

    async fn forward(&self, message: Message, cancel: &CancellationToken) -> bool {
        let report = Report {
            conn: self.conn,
            pid: self.pid,
            message,
        };
        tokio::select! {
            () = cancel.cancelled() => false,
            sent = self.reports.send(report) => match sent {
                Ok(()) => true,
                Err(_closed) => {
                    tracing::debug!("helper translator stopped, reporter closed");
                    false
                }
            },
        }
    }
}

/// What an open operation produced.
#[derive(Debug)]
enum Opened {
    /// A pending signal with these fields.
    Signal(Signal),
    /// A notice for this pid.
    Notice(u32),
    /// Nothing: its card slot needs no touch, or its askpass reporter has
    /// no detail or no known parent.
    Nothing,
}

/// An open operation.
#[derive(Debug)]
struct OpenOp {
    started: Instant,
    opened: Opened,
}

/// Cached card attributes.
#[derive(Debug)]
struct Card {
    state: CardState,
    read_at: Instant,
}

/// Turns reports into signals and notices, gating scdaemon operations on the card's UIF.
struct Translator {
    agent: Option<AgentPaths>,
    outputs: HelperOutputs,
    tracker: TaskTracker,
    cancel: CancellationToken,
    /// Keyed by connection, origin name and `seq`.
    ops: BTreeMap<(u64, &'static str, u32), OpenOp>,
    next_channel: u32,
    card: Option<Card>,
    /// A UIF read is due once no operation is open.
    stale: bool,
    reading: Option<JoinHandle<Result<CardState, AgentError>>>,
    /// Connection of the latest scdaemon report; a new one may mean a new card.
    scdaemon_conn: Option<u64>,
    /// An scdaemon operation started since the last UIF read; the TTL
    /// refresh runs only then, so an unused card is never touched.
    used: bool,
    read_limit: RateLimit,
}

impl Translator {
    fn new(
        agent: Option<AgentPaths>,
        outputs: HelperOutputs,
        tracker: TaskTracker,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            agent,
            outputs,
            tracker,
            cancel,
            ops: BTreeMap::new(),
            next_channel: 0,
            card: None,
            stale: false,
            reading: None,
            scdaemon_conn: None,
            used: false,
            read_limit: RateLimit::new(WARN_INTERVAL),
        }
    }

    async fn run(mut self, mut reports: mpsc::Receiver<Report>, keepalive: Duration) {
        let period = keepalive.max(Duration::from_millis(1));
        let mut ticks = tokio::time::interval_at(Instant::now() + period, period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let cancel = self.cancel.clone();
        loop {
            let task = self.reading.as_mut();
            let reading = async move {
                match task {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            };
            let alive = tokio::select! {
                () = cancel.cancelled() => false,
                report = reports.recv() => match report {
                    Some(report) => self.report(report).await,
                    None => false,
                },
                _ = ticks.tick() => self.keepalive().await,
                read = reading => {
                    self.reading = None;
                    self.store(read);
                    true
                }
            };
            if !alive {
                break;
            }
            self.maybe_read();
        }
        if let Some(task) = self.reading.take() {
            task.abort();
        }
        tracing::debug!("helper translator stopped");
    }

    /// Starts a UIF read if one is due, none runs, and no operation is open.
    fn maybe_read(&mut self) {
        if self
            .card
            .as_ref()
            .is_some_and(|card| card.read_at.elapsed() >= UIF_TTL)
            && self.used
        {
            self.stale = true;
        }
        if !self.stale || self.reading.is_some() || !self.ops.is_empty() {
            return;
        }
        self.stale = false;
        self.used = false;
        let Some(agent) = &self.agent else {
            return;
        };
        let socket = agent.agent.clone();
        let cancel = self.cancel.clone();
        let task = async move {
            tokio::select! {
                () = cancel.cancelled() => Err(AgentError::Closed),
                read = agent::read_card(&socket) => read,
            }
        };
        self.reading = Some(
            self.tracker
                .spawn(task.instrument(tracing::debug_span!("uif_read"))),
        );
    }

    fn store(&mut self, read: Result<Result<CardState, AgentError>, tokio::task::JoinError>) {
        match read {
            Ok(Ok(state)) => {
                tracing::debug!(
                    uif = ?state.uif,
                    manufacturer = state.manufacturer,
                    "card attributes read"
                );
                self.card = Some(Card {
                    state,
                    read_at: Instant::now(),
                });
            }
            Ok(Err(error)) => {
                // Unknown values report every operation until the next read.
                self.card = Some(Card {
                    state: CardState::default(),
                    read_at: Instant::now(),
                });
                self.read_limit
                    .log(Instant::now().into_std(), |suppressed| {
                        tracing::debug!(
                            error = &error as &dyn Error,
                            suppressed,
                            "cannot read card attributes"
                        );
                    });
            }
            Err(error) => {
                tracing::warn!(error = &error as &dyn Error, "card attribute read failed");
            }
        }
    }

    /// Sends a progress signal for every signalled open operation and times
    /// out the open operations older than [`MAX_OP_AGE`]; returns whether
    /// the outputs are still open.
    async fn keepalive(&mut self) -> bool {
        let expired: Vec<_> = self
            .ops
            .iter()
            .filter(|(_, op)| op.started.elapsed() >= MAX_OP_AGE)
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            let Some(op) = self.ops.remove(&key) else {
                continue;
            };
            tracing::warn!("operation open too long, timed out");
            if !self.close(op.opened, Outcome::TimedOut).await {
                return false;
            }
        }
        let progress: Vec<Signal> = self
            .ops
            .values()
            .filter_map(|op| match &op.opened {
                Opened::Signal(signal) => Some(Signal {
                    kind: SignalKind::Progress,
                    ..signal.clone()
                }),
                Opened::Notice(_) | Opened::Nothing => None,
            })
            .collect();
        for signal in progress {
            if !self.send(signal).await {
                return false;
            }
        }
        true
    }

    async fn report(&mut self, report: Report) -> bool {
        let Report { conn, pid, message } = report;
        match message {
            Message::Start {
                origin,
                seq,
                op,
                detail,
            } => {
                if origin == Origin::Scdaemon {
                    self.used = true;
                }
                if origin == Origin::Scdaemon && self.scdaemon_conn != Some(conn) {
                    self.scdaemon_conn = Some(conn);
                    let recent = self
                        .card
                        .as_ref()
                        .is_some_and(|card| card.read_at.elapsed() < MIN_READ_INTERVAL);
                    self.stale |= !recent;
                }
                let (opened, sent) = match origin {
                    Origin::Scdaemon => {
                        let opened = self.scdaemon_start(op).await;
                        let sent = match &opened {
                            Opened::Signal(signal) => self.send(signal.clone()).await,
                            Opened::Notice(_) | Opened::Nothing => true,
                        };
                        (opened, sent)
                    }
                    Origin::Askpass => match askpass_start(pid, detail).await {
                        Some(notice @ Notice::Started { pid, .. }) => {
                            (Opened::Notice(pid), self.notify(notice).await)
                        }
                        Some(Notice::Ended { .. }) | None => (Opened::Nothing, true),
                    },
                };
                self.ops.insert(
                    (conn, origin.as_str(), seq),
                    OpenOp {
                        started: Instant::now(),
                        opened,
                    },
                );
                sent
            }
            Message::End {
                origin,
                seq,
                outcome,
            } => match self.ops.remove(&(conn, origin.as_str(), seq)) {
                Some(op) => self.close(op.opened, outcome).await,
                None => true,
            },
        }
    }

    /// Sends what ends an operation that `opened`.
    async fn close(&self, opened: Opened, outcome: Outcome) -> bool {
        match opened {
            Opened::Signal(signal) => {
                self.send(Signal {
                    kind: SignalKind::Resolved(outcome),
                    ..signal
                })
                .await
            }
            Opened::Notice(pid) => self.notify(Notice::Ended { pid }).await,
            Opened::Nothing => true,
        }
    }

    /// Returns the pending signal of a new scdaemon operation, or nothing
    /// when its card slot needs no touch.
    async fn scdaemon_start(&mut self, op: Op) -> Opened {
        if !self.requires_touch(op) {
            return Opened::Nothing;
        }
        let channel = self.next_channel;
        self.next_channel = self.next_channel.wrapping_add(1);
        let source = if op == Op::Auth && self.ssh_connected().await {
            Source::Ssh
        } else {
            Source::Gpg
        };
        tracing::debug!(
            source = source.as_str(),
            channel,
            op = op.as_str(),
            "operation started"
        );
        Opened::Signal(Signal {
            device: self.openpgp_device(),
            source,
            class: SignalClass::Activity,
            kind: SignalKind::Pending {
                method: Method::OpenPgp,
                op: Some(op),
            },
            channel: Some(channel),
            pids: Vec::new(),
        })
    }

    /// Reports whether `op` waits for a touch; unknown counts as yes.
    fn requires_touch(&mut self, op: Op) -> bool {
        let Some(card) = &self.card else {
            self.stale = true;
            tracing::debug!("UIF not read yet, reporting the operation");
            return true;
        };
        let uif = assuan::slot(op).and_then(|slot| {
            card.state
                .uif
                .get(usize::from(slot).checked_sub(1)?)
                .copied()
                .flatten()
        });
        if let Some(uif) = uif {
            tracing::debug!(op = op.as_str(), ?uif, "card slot UIF");
            uif.requires_touch()
        } else {
            tracing::debug!(op = op.as_str(), "UIF unknown, reporting the operation");
            true
        }
    }

    /// Reports whether a client is connected to gpg-agent's ssh socket.
    async fn ssh_connected(&self) -> bool {
        let Some(agent) = &self.agent else {
            return false;
        };
        let ssh = agent.ssh.clone();
        let check = tokio::task::spawn_blocking(move || sockdiag::connections(&[ssh.as_path()]));
        match timeout(SSH_CHECK_TIMEOUT, check).await {
            Ok(Ok(Ok(connections))) => !connections.is_empty(),
            Ok(Ok(Err(error))) => {
                tracing::debug!(
                    error = &error as &dyn Error,
                    "cannot list ssh agent clients"
                );
                false
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = &error as &dyn Error,
                    "ssh agent client check failed"
                );
                false
            }
            Err(_elapsed) => {
                tracing::debug!("ssh agent client check timed out");
                false
            }
        }
    }

    fn openpgp_device(&self) -> Device {
        let vendor = self
            .card
            .as_ref()
            .and_then(|card| card.state.manufacturer)
            .and_then(assuan::manufacturer_name);
        Device {
            id: DeviceId(OPENPGP_DEVICE.to_owned()),
            kind: DeviceKind::OpenPgp,
            transport: Transport::Other,
            vid: None,
            pid: None,
            vendor: vendor.map(str::to_owned),
            model: None,
            product: None,
        }
    }

    async fn send(&self, signal: Signal) -> bool {
        tokio::select! {
            () = self.cancel.cancelled() => false,
            sent = self.outputs.signals.send(signal) => match sent {
                Ok(()) => true,
                Err(_closed) => {
                    tracing::debug!("signal receiver closed");
                    false
                }
            },
        }
    }

    async fn notify(&self, notice: Notice) -> bool {
        tokio::select! {
            () = self.cancel.cancelled() => false,
            sent = self.outputs.notices.send(notice) => match sent {
                Ok(()) => true,
                Err(_closed) => {
                    tracing::debug!("notice receiver closed");
                    false
                }
            },
        }
    }
}

/// Returns the notice of an askpass operation with `detail`, reported by
/// `pid`, or `None` when there is no detail or the parent is unknown.
async fn askpass_start(pid: Option<u32>, detail: Option<String>) -> Option<Notice> {
    let (Some(pid), Some(detail)) = (pid, detail) else {
        tracing::debug!("askpass report without pid or detail ignored");
        return None;
    };
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let parent = match tokio::task::spawn_blocking(move || read_parent(&path)).await {
        Ok(Ok(parent)) => parent,
        Ok(Err(error)) => {
            tracing::debug!(
                pid,
                error = &error as &dyn Error,
                "cannot read askpass parent"
            );
            None
        }
        Err(error) => {
            tracing::warn!(error = &error as &dyn Error, "askpass parent lookup failed");
            None
        }
    };
    let Some(parent) = parent else {
        tracing::debug!(pid, "askpass reporter has no parent; ignored");
        return None;
    };
    tracing::debug!(pid = parent, "askpass notice");
    Some(Notice::Started {
        pid: parent,
        detail,
    })
}

/// Reads the parent pid, field 4 of a `stat` file, after the parenthesized
/// `comm`; `None` for init, an orphan, or an unparsable file.
fn read_parent(path: &Path) -> std::io::Result<Option<u32>> {
    let mut stat = String::new();
    std::fs::File::open(path)?
        .take(STAT_MAX)
        .read_to_string(&mut stat)?;
    let Some(field) = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(1))
    else {
        return Ok(None);
    };
    match field.parse::<u32>() {
        Ok(ppid) if ppid > 1 => Ok(Some(ppid)),
        Ok(_) => Ok(None),
        Err(error) => {
            tracing::trace!(error = &error as &dyn Error, "parent pid unparsable");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use tokio::io::AsyncWriteExt as _;

    use super::*;
    use crate::agent::tests::fake_agent;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Ipc(#[from] IpcError),
        #[error(transparent)]
        Agent(#[from] crate::agent::tests::TestError),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
        #[error("nothing arrived in time")]
        NothingArrived,
        #[error("read returned {0} bytes instead of blocking")]
        NotBlocked(usize),
        #[error(transparent)]
        Elapsed(#[from] tokio::time::error::Elapsed),
        #[error("unexpected signal {0:?}")]
        Unexpected(Box<Signal>),
    }

    type TestResult = Result<(), TestError>;

    const WAIT: Duration = Duration::from_secs(10);

    struct Fixture {
        _dir: tempfile::TempDir,
        runtime: PathBuf,
        helper: Helper,
        rx: mpsc::Receiver<Signal>,
        notices: mpsc::Receiver<Notice>,
    }

    impl Fixture {
        async fn start(agent: Option<AgentPaths>, keepalive: Duration) -> Result<Self, TestError> {
            let dir = tempfile::tempdir()?;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
            let runtime = dir.path().to_owned();
            let (signals, rx) = mpsc::channel(16);
            let (notices_tx, notices) = mpsc::channel(16);
            let helper = Helper::spawn(
                HelperConfig {
                    runtime_dir: runtime.clone(),
                    agent,
                    keepalive,
                },
                HelperOutputs {
                    signals,
                    notices: notices_tx,
                },
                &CancellationToken::new(),
            )
            .await?;
            Ok(Self {
                _dir: dir,
                runtime,
                helper,
                rx,
                notices,
            })
        }

        async fn connect(&self) -> Result<UnixStream, TestError> {
            Ok(UnixStream::connect(self.runtime.join("touchcue/helper.sock")).await?)
        }

        async fn next(&mut self) -> Result<Signal, TestError> {
            timeout(WAIT, self.rx.recv())
                .await?
                .ok_or(TestError::NothingArrived)
        }

        async fn notice(&mut self) -> Result<Notice, TestError> {
            timeout(WAIT, self.notices.recv())
                .await?
                .ok_or(TestError::NothingArrived)
        }

        /// Returns the next signal that is not progress.
        async fn next_change(&mut self) -> Result<Signal, TestError> {
            loop {
                let signal = self.next().await?;
                if signal.kind != SignalKind::Progress {
                    return Ok(signal);
                }
            }
        }
    }

    fn pending(op: Op) -> SignalKind {
        SignalKind::Pending {
            method: Method::OpenPgp,
            op: Some(op),
        }
    }

    #[test]
    fn only_the_same_user_is_admitted() {
        assert!(admitted(Some(1000), 1000));
        assert!(!admitted(Some(0), 1000));
        assert!(!admitted(None, 1000));
    }

    #[test]
    fn parent_is_field_four_after_comm() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stat");
        fs::write(&path, "42 (a) b (c) S 41 42 42 0 -1")?;
        assert_eq!(read_parent(&path)?, Some(41));
        fs::write(&path, "42 (init) S 1 42")?;
        assert_eq!(read_parent(&path)?, None);
        fs::write(&path, "42 (x) S")?;
        assert_eq!(read_parent(&path)?, None);
        assert!(matches!(
            read_parent(&dir.path().join("gone")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
        Ok(())
    }

    #[test]
    fn notice_debug_omits_detail() {
        let notice = Notice::Started {
            pid: 7,
            detail: "SHA256:secret".to_owned(),
        };
        assert!(!format!("{notice:?}").contains("secret"));
    }

    #[tokio::test]
    async fn askpass_becomes_a_notice_after_a_malformed_line() -> TestResult {
        let mut fx = Fixture::start(None, Duration::from_secs(60)).await?;
        let mut client = fx.connect().await?;
        client
            .write_all(b"v1 start nobody 1 sign\n\xff\xfe\n")
            .await?;
        client
            .write_all(b"v1 start askpass 1 auth SHA256:abc%20to%20host\n")
            .await?;
        let parent = std::os::unix::process::parent_id();
        assert_eq!(
            fx.notice().await?,
            Notice::Started {
                pid: parent,
                detail: "SHA256:abc to host".to_owned()
            }
        );
        client.write_all(b"v1 end askpass 1 touched\n").await?;
        assert_eq!(fx.notice().await?, Notice::Ended { pid: parent });
        assert_eq!(fx.rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn open_operation_of_a_closed_connection_fails() -> TestResult {
        let mut fx = Fixture::start(None, Duration::from_secs(60)).await?;
        let mut first = fx.connect().await?;
        let mut second = fx.connect().await?;
        first.write_all(b"v1 start scdaemon 1 sign\n").await?;
        let a = fx.next().await?;
        second.write_all(b"v1 start scdaemon 1 decrypt\n").await?;
        let b = fx.next().await?;
        assert_eq!(a.source, Source::Gpg);
        assert_eq!(a.kind, pending(Op::Sign));
        assert_eq!(a.device.kind, DeviceKind::OpenPgp);
        assert_eq!(a.device.id.0, OPENPGP_DEVICE);
        assert_ne!(a.channel, b.channel);
        drop(first);
        let failed = fx.next().await?;
        assert_eq!(failed.kind, SignalKind::Resolved(Outcome::Failed));
        assert_eq!(failed.channel, a.channel);
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn open_operation_is_kept_alive() -> TestResult {
        let mut fx = Fixture::start(None, Duration::from_millis(20)).await?;
        let mut client = fx.connect().await?;
        client.write_all(b"v1 start scdaemon 7 auth\n").await?;
        let started = fx.next().await?;
        let progress = fx.next().await?;
        assert_eq!(progress.kind, SignalKind::Progress);
        assert_eq!(progress.channel, started.channel);
        client.write_all(b"v1 end scdaemon 7 cancelled\n").await?;
        let ended = fx.next_change().await?;
        assert_eq!(ended.kind, SignalKind::Resolved(Outcome::Cancelled));
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clients_beyond_the_limit_are_closed() -> TestResult {
        let fx = Fixture::start(None, Duration::from_secs(60)).await?;
        let mut clients = Vec::new();
        for _ in 0..MAX_CLIENTS {
            clients.push(fx.connect().await?);
        }
        let mut extra = fx.connect().await?;
        let mut byte = [0; 1];
        let read = timeout(WAIT, extra.read(&mut byte)).await;
        assert!(matches!(read, Ok(Ok(0))), "{read:?}");
        let first = clients.first_mut().ok_or(TestError::NothingArrived)?;
        let still_open = timeout(Duration::from_millis(100), first.read(&mut byte)).await;
        match still_open {
            Err(_elapsed) => {}
            Ok(read) => return Err(TestError::NotBlocked(read?)),
        }
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn slot_without_touch_is_not_reported() -> TestResult {
        let dir = tempfile::tempdir()?;
        let agent_socket = dir.path().join("S.gpg-agent");
        let agent = fake_agent(
            &agent_socket,
            Some([Some("%00+"), Some("%01+"), Some("%FF+")]),
        )?;
        let paths = AgentPaths {
            agent: agent_socket,
            ssh: dir.path().join("S.gpg-agent.ssh"),
            homedir: dir.path().to_owned(),
        };
        let mut fx = Fixture::start(Some(paths), Duration::from_secs(60)).await?;
        let mut client = fx.connect().await?;
        // Before the first UIF read every operation is reported.
        client.write_all(b"v1 start scdaemon 1 sign\n").await?;
        assert_eq!(fx.next().await?.kind, pending(Op::Sign));
        client.write_all(b"v1 end scdaemon 1 touched\n").await?;
        assert_eq!(
            fx.next().await?.kind,
            SignalKind::Resolved(Outcome::Touched)
        );
        timeout(WAIT, agent).await???;
        // The read is stored before the next report is handled.
        tokio::time::sleep(Duration::from_millis(100)).await;
        client
            .write_all(b"v1 start scdaemon 2 sign\nv1 end scdaemon 2 touched\n")
            .await?;
        client
            .write_all(b"v1 start scdaemon 3 auth\nv1 end scdaemon 3 touched\n")
            .await?;
        client.write_all(b"v1 start scdaemon 4 decrypt\n").await?;
        let decrypt = fx.next().await?;
        if decrypt.kind != pending(Op::Decrypt) {
            return Err(TestError::Unexpected(Box::new(decrypt)));
        }
        assert_eq!(decrypt.device.vendor.as_deref(), Some("Yubico"));
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn uif_ttl_refresh_needs_a_report_since_the_last_read() {
        tokio::time::pause();
        let (signals, _signals_rx) = mpsc::channel(1);
        let (notices, _notices_rx) = mpsc::channel(1);
        let mut translator = Translator::new(
            Some(AgentPaths {
                agent: PathBuf::from("/nonexistent/S.gpg-agent"),
                ssh: PathBuf::from("/nonexistent/S.gpg-agent.ssh"),
                homedir: PathBuf::from("/nonexistent"),
            }),
            HelperOutputs { signals, notices },
            TaskTracker::new(),
            CancellationToken::new(),
        );
        translator.card = Some(Card {
            state: CardState::default(),
            read_at: Instant::now(),
        });
        tokio::time::advance(UIF_TTL + Duration::from_secs(1)).await;
        translator.maybe_read();
        assert!(translator.reading.is_none(), "unused card was read");
        translator.used = true;
        translator.maybe_read();
        assert!(translator.reading.is_some(), "used card was not read");
        assert!(!translator.used);
        if let Some(task) = translator.reading.take() {
            task.abort();
        }
    }

    #[tokio::test]
    async fn starts_beyond_the_rate_limit_are_dropped() -> TestResult {
        let mut fx = Fixture::start(None, Duration::from_secs(60)).await?;
        let mut client = fx.connect().await?;
        let total = MAX_STARTS + 5;
        for seq in 0..total {
            client
                .write_all(
                    format!("v1 start scdaemon {seq} sign\nv1 end scdaemon {seq} touched\n")
                        .as_bytes(),
                )
                .await?;
        }
        let mut pending = 0;
        for _ in 0..MAX_STARTS * 2 {
            if matches!(fx.next().await?.kind, SignalKind::Pending { .. }) {
                pending += 1;
            }
        }
        assert_eq!(pending, MAX_STARTS);
        client.write_all(b"v1 start askpass 99 auth\n").await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fx.rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        fx.helper.stop().await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn idle_connection_is_closed() -> TestResult {
        let fx = Fixture::start(None, Duration::from_secs(60)).await?;
        let mut client = fx.connect().await?;
        let mut byte = [0; 1];
        let read = timeout(IDLE_TIMEOUT * 2, client.read(&mut byte)).await;
        assert!(matches!(read, Ok(Ok(0))), "{read:?}");
        fx.helper.stop().await?;
        Ok(())
    }
}
