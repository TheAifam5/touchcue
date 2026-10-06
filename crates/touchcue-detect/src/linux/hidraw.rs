//! Watcher that turns FIDO hidraw input reports into signals.
//!
//! Every opened hidraw file receives its own copy of each input report, so
//! reading alongside browsers and ssh does not take reports from them.

use std::collections::BTreeMap;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, TryAcquireError};
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};
use tokio_util::sync::{CancellationToken, DropGuard};
use touchcue_core::ctaphid::{self, Frame};
use touchcue_core::{Device, Method, RateLimit, Signal, SignalClass, SignalKind, Source};
use tracing::Instrument;

use crate::DetectError;
use crate::linux::sysfs::{fido_device, is_node_name, list_fido};

/// Capacity of the signal channel the caller should create.
///
/// The watcher waits for room instead of dropping signals, so a resolution
/// is never lost; the per-node rate limits bound what it can queue.
pub const SIGNAL_QUEUE: usize = 256;
/// Capacity of the device event channel the caller should create; an
/// event that finds it full is dropped with a warning.
pub const DEVICE_QUEUE: usize = 16;
/// HID report size of CTAPHID packets.
const REPORT_BYTES: usize = 64;
/// Reports read from one node per wakeup before other tasks get a turn.
const MAX_REPORTS_PER_WAKEUP: usize = 64;
/// Room for at least one inotify event with a `NAME_MAX` file name.
const INOTIFY_BUFFER_BYTES: usize = 4096;
/// Most hidraw nodes watched at once.
const MAX_NODES: usize = 32;
/// Shortest interval between two identical signals from one node.
const REPEAT_INTERVAL: Duration = Duration::from_millis(250);
/// Signals one node may emit in a burst.
const BUCKET_CAPACITY: u32 = 20;
/// Time to regain one emit token: 20 tokens per second.
const TOKEN_INTERVAL: Duration = Duration::from_millis(50);
/// Shortest interval between two rate limit log events for one node.
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(1);
/// Longest wait for one sysfs lookup or scan. A lookup not finished in time
/// is abandoned and keeps its blocking thread and the scan slot until it
/// returns; further lookups are skipped until then.
const SCAN_TIMEOUT: Duration = Duration::from_secs(2);
/// Delay before a full rescan replaces a skipped or abandoned lookup.
const RESCAN_RETRY: Duration = Duration::from_secs(1);
/// Shortest interval between two log events for skipped lookups.
const SCAN_BUSY_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Shortest interval between two log events for dropped device events.
const DEVICE_DROP_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Opens a node path; `Ok(None)` means the path is not a node to watch.
type Opener = fn(&Path) -> rustix::io::Result<Option<OwnedFd>>;

/// A FIDO device the watcher started or stopped watching.
///
/// Devices present when the watcher starts are added too. After the inotify
/// queue overflows, every watched device is removed and added again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Added(Device),
    Removed(Device),
}

/// Running watcher of FIDO hidraw nodes.
///
/// Dropping it cancels the watcher without waiting; [`Watcher::stop`] also
/// waits for every task to end. Neither cancels the parent token given to [`spawn`].
#[derive(Debug)]
pub struct Watcher {
    cancel: CancellationToken,
    task: JoinHandle<Result<(), DetectError>>,
    _guard: DropGuard,
}

/// Starts watching the FIDO hidraw nodes of `dev_root`, sending a signal per
/// CTAPHID report of interest to `tx`, and a [`DeviceEvent`] per node it
/// starts or stops watching to `devices`, if given.
///
/// `sys_root` is the sysfs mount point, normally `/sys`; `dev_root` the device
/// directory, normally `/dev`. Nodes present now and nodes added later are
/// watched until they disappear. The watcher ends when `parent` is cancelled,
/// the receiver of `tx` is dropped, or reading inotify events fails; every
/// clone of `tx` it holds is dropped when it ends. Device events are sent
/// without waiting: one that finds `devices` full is dropped. Must be called
/// within a Tokio runtime with I/O and time enabled.
///
/// # Errors
///
/// Returns [`DetectError::Io`] when the inotify watch cannot be created or
/// registered with the runtime.
pub fn spawn(
    sys_root: PathBuf,
    dev_root: PathBuf,
    tx: Sender<Signal>,
    devices: Option<Sender<DeviceEvent>>,
    parent: &CancellationToken,
) -> Result<Watcher, DetectError> {
    spawn_with(sys_root, dev_root, tx, devices, parent, open_node)
}

fn spawn_with(
    sys_root: PathBuf,
    dev_root: PathBuf,
    tx: Sender<Signal>,
    devices: Option<Sender<DeviceEvent>>,
    parent: &CancellationToken,
    open: Opener,
) -> Result<Watcher, DetectError> {
    let cancel = parent.child_token();
    let inotify = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)
        .map_err(io("cannot create inotify instance"))?;
    inotify::add_watch(
        &inotify,
        &dev_root,
        WatchFlags::CREATE | WatchFlags::DELETE | WatchFlags::ATTRIB,
    )
    .map_err(io("cannot watch device directory"))?;
    let inotify = AsyncFd::new(inotify).map_err(|source| DetectError::Io {
        context: "cannot register inotify instance",
        source,
    })?;
    let state = State::new(sys_root, dev_root, tx, cancel.clone(), open).reporting(devices);
    let task = tokio::spawn(
        state
            .run(inotify)
            .instrument(tracing::info_span!("hidraw_watcher")),
    );
    Ok(Watcher {
        _guard: cancel.clone().drop_guard(),
        cancel,
        task,
    })
}

impl Watcher {
    /// Cancels the watcher and waits for it and every node task to end.
    ///
    /// # Errors
    ///
    /// Returns the error the watcher ended with, or [`DetectError::Join`]
    /// when a task panicked.
    pub async fn stop(self) -> Result<(), DetectError> {
        self.cancel.cancel();
        self.task.await?
    }
}

fn io(context: &'static str) -> impl FnOnce(Errno) -> DetectError {
    move |errno| DetectError::Io {
        context,
        source: errno.into(),
    }
}

/// Returns `duration` in whole milliseconds, saturating at `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(duration.subsec_millis()))
}

/// Returns the current time from the Tokio clock, so that a paused clock applies.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Maps a CTAPHID frame to the signal it reports, if any.
fn kind_for(frame: Frame) -> Option<SignalKind> {
    match frame {
        Frame::KeepaliveUpNeeded => Some(SignalKind::Pending {
            method: Method::Fido2,
            op: None,
        }),
        Frame::U2fConditionsNotSatisfied => Some(SignalKind::Pending {
            method: Method::U2f,
            op: None,
        }),
        Frame::KeepaliveProcessing => Some(SignalKind::Progress),
        Frame::Done(outcome) => Some(SignalKind::Resolved(outcome)),
        Frame::Other => None,
    }
}

/// Returns whether a signal differs from the last one emitted for its node, or
/// that one was emitted at least [`REPEAT_INTERVAL`] before `now`.
fn should_emit(last: Option<&Emitted>, cid: u32, kind: SignalKind, now: Instant) -> bool {
    last.is_none_or(|last| {
        last.cid != cid
            || last.kind != kind
            || now.saturating_duration_since(last.at) >= REPEAT_INTERVAL
    })
}

/// Opens a hidraw node for reading without following symlinks.
///
/// Returns `None` when `path` is not a character device.
fn open_node(path: &Path) -> rustix::io::Result<Option<OwnedFd>> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    Ok(FileType::from_raw_mode(stat.st_mode)
        .is_char_device()
        .then_some(fd))
}

/// Token bucket bounding the signals one node emits.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    /// Time up to which refills are accounted for.
    refilled: Instant,
}

impl Bucket {
    fn full(now: Instant) -> Self {
        Self {
            tokens: BUCKET_CAPACITY,
            refilled: now,
        }
    }

    /// Refills for the time elapsed up to `now`, then takes one token if any.
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled);
        let earned = (millis(elapsed) / millis(TOKEN_INTERVAL)).min(u64::from(BUCKET_CAPACITY));
        let earned = match u32::try_from(earned) {
            Ok(earned) => earned,
            Err(error) => {
                tracing::trace!(
                    error = &error as &dyn std::error::Error,
                    "token count saturated"
                );
                BUCKET_CAPACITY
            }
        };
        if earned > 0 {
            self.tokens = self.tokens.saturating_add(earned).min(BUCKET_CAPACITY);
            self.refilled = if self.tokens == BUCKET_CAPACITY {
                now
            } else {
                self.refilled
                    .checked_add(TOKEN_INTERVAL.saturating_mul(earned))
                    .unwrap_or(now)
            };
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// Rate limits and their logging for one node.
struct Limiter {
    last: Option<Emitted>,
    bucket: Bucket,
    /// When dropping signals over the rate limit was last logged.
    drop_logged: Option<Instant>,
}

impl Limiter {
    fn new(now: Instant) -> Self {
        Self {
            last: None,
            bucket: Bucket::full(now),
            drop_logged: None,
        }
    }

    /// Returns the channel and kind of a report that passes both limits.
    fn accept(&mut self, report: &[u8], now: Instant) -> Option<(u32, SignalKind)> {
        let (cid, frame) = ctaphid::parse(report)?;
        let kind = kind_for(frame)?;
        if !should_emit(self.last.as_ref(), cid, kind, now) {
            return None;
        }
        if !self.bucket.take(now) {
            if self
                .drop_logged
                .is_none_or(|at| now.saturating_duration_since(at) >= DROP_LOG_INTERVAL)
            {
                tracing::debug!("dropping signals over the rate limit");
                self.drop_logged = Some(now);
            }
            return None;
        }
        self.last = Some(Emitted { cid, kind, at: now });
        Some((cid, kind))
    }
}

/// Last signal emitted for a node.
#[derive(Debug, Clone, Copy)]
struct Emitted {
    cid: u32,
    kind: SignalKind,
    at: Instant,
}

/// Reads one node until it disappears, `cancel` fires or `tx` closes.
///
/// Readiness is edge-triggered: the node is read until `EAGAIN` before the
/// readiness is cleared. After [`MAX_REPORTS_PER_WAKEUP`] reports the task
/// yields with the readiness kept, so it continues on its next turn.
async fn read_node(
    fd: AsyncFd<OwnedFd>,
    device: Device,
    tx: Sender<Signal>,
    cancel: CancellationToken,
) {
    let mut limiter = Limiter::new(now());
    let mut buf = [0u8; REPORT_BYTES];
    loop {
        let mut guard = tokio::select! {
            () = cancel.cancelled() => return,
            ready = fd.readable() => match ready {
                Ok(guard) => guard,
                Err(error) => {
                    tracing::debug!(error = &error as &dyn std::error::Error, "cannot wait for hidraw node");
                    return;
                }
            },
        };
        let mut reads = 0;
        loop {
            if reads == MAX_REPORTS_PER_WAKEUP {
                tokio::task::yield_now().await;
                break;
            }
            match rustix::io::read(guard.get_inner(), &mut buf) {
                Ok(0) => {
                    tracing::debug!("hidraw node closed");
                    return;
                }
                Ok(len) => {
                    reads += 1;
                    let Some((cid, kind)) = buf
                        .get(..len)
                        .and_then(|report| limiter.accept(report, now()))
                    else {
                        continue;
                    };
                    let signal = Signal {
                        device: device.clone(),
                        source: Source::Fido,
                        class: SignalClass::Asserted,
                        kind,
                        channel: Some(cid),
                        pids: Vec::new(),
                    };
                    tokio::select! {
                        sent = tx.send(signal) => if let Err(_closed) = sent {
                            tracing::debug!("signal receiver closed");
                            return;
                        },
                        () = cancel.cancelled() => return,
                    }
                }
                Err(Errno::AGAIN) => {
                    guard.clear_ready();
                    break;
                }
                Err(Errno::INTR) => {}
                Err(errno) => {
                    tracing::debug!(
                        error = &errno as &dyn std::error::Error,
                        "cannot read hidraw node"
                    );
                    return;
                }
            }
        }
    }
}

/// A watched node.
struct NodeHandle {
    task: Id,
    cancel: CancellationToken,
    device: Device,
}

struct State {
    sys_root: PathBuf,
    dev_root: PathBuf,
    tx: Sender<Signal>,
    /// Receives a [`DeviceEvent`] per node opened or forgotten.
    devices: Option<Sender<DeviceEvent>>,
    open: Opener,
    /// Cancelled by the caller to stop the watcher.
    cancel: CancellationToken,
    /// Parent of every node token; cancelled when the watcher ends.
    nodes_cancel: CancellationToken,
    /// Watched nodes by file name under `dev_root`.
    nodes: BTreeMap<String, NodeHandle>,
    tasks: JoinSet<()>,
    /// Held by the one sysfs lookup in flight, including an abandoned one.
    scan_slot: Arc<Semaphore>,
    /// When to rescan after a lookup was skipped or abandoned.
    rescan_at: Option<Instant>,
    busy_limit: RateLimit,
    device_drop_limit: RateLimit,
}

impl State {
    fn new(
        sys_root: PathBuf,
        dev_root: PathBuf,
        tx: Sender<Signal>,
        cancel: CancellationToken,
        open: Opener,
    ) -> Self {
        Self {
            sys_root,
            dev_root,
            tx,
            devices: None,
            open,
            nodes_cancel: cancel.child_token(),
            cancel,
            nodes: BTreeMap::new(),
            tasks: JoinSet::new(),
            scan_slot: Arc::new(Semaphore::new(1)),
            rescan_at: None,
            busy_limit: RateLimit::new(SCAN_BUSY_LOG_INTERVAL),
            device_drop_limit: RateLimit::new(DEVICE_DROP_LOG_INTERVAL),
        }
    }

    /// Reports the nodes opened and forgotten to `devices`.
    fn reporting(mut self, devices: Option<Sender<DeviceEvent>>) -> Self {
        self.devices = devices;
        self
    }

    /// Sends `event` to `devices` without waiting; a full channel drops it
    /// with a rate-limited warning.
    fn report(&mut self, event: DeviceEvent) {
        let Some(devices) = &self.devices else {
            return;
        };
        match devices.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.device_drop_limit.log(now(), |suppressed| {
                    tracing::warn!(
                        limit = DEVICE_QUEUE,
                        suppressed,
                        "device event queue full; dropping an event"
                    );
                });
            }
            Err(TrySendError::Closed(_)) => {
                tracing::debug!("device event receiver closed");
            }
        }
    }

    /// Watches until cancelled, then stops every node task and waits for it.
    async fn run(mut self, inotify: AsyncFd<OwnedFd>) -> Result<(), DetectError> {
        let result = self.watch(&inotify).await;
        self.nodes_cancel.cancel();
        let mut first = None;
        if let Err(error) = result {
            first = Some(error);
        }
        while let Some(joined) = self.tasks.join_next().await {
            if let Err(error) = joined {
                if first.is_some() {
                    tracing::warn!(
                        error = &error as &dyn std::error::Error,
                        "hidraw node task failed"
                    );
                } else {
                    first = Some(error.into());
                }
            }
        }
        tracing::debug!("hidraw watcher stopped");
        first.map_or(Ok(()), Err)
    }

    async fn watch(&mut self, inotify: &AsyncFd<OwnedFd>) -> Result<(), DetectError> {
        self.rescan().await?;
        loop {
            let wake = tokio::time::Instant::from_std(self.rescan_at.unwrap_or_else(now));
            tokio::select! {
                () = self.cancel.cancelled() => return Ok(()),
                () = self.tx.closed() => return Ok(()),
                ready = inotify.readable() => {
                    let mut guard = ready.map_err(|source| DetectError::Io {
                        context: "cannot wait for inotify events",
                        source,
                    })?;
                    let (events, overflow) = read_inotify(guard.get_inner())?;
                    guard.clear_ready();
                    self.apply(events, overflow).await?;
                }
                Some(joined) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    self.reap(joined)?;
                }
                () = tokio::time::sleep_until(wake), if self.rescan_at.is_some() => {
                    self.rescan().await?;
                }
            }
        }
    }

    /// Forgets a node whose task ended on its own.
    fn reap(&mut self, joined: Result<(Id, ()), JoinError>) -> Result<(), DetectError> {
        let task = match &joined {
            Ok((task, ())) => *task,
            Err(error) => error.id(),
        };
        let ended = self
            .nodes
            .iter()
            .find(|(_, node)| node.task == task)
            .map(|(name, _)| name.clone());
        if let Some(node) = ended.and_then(|name| self.nodes.remove(&name)) {
            self.report(DeviceEvent::Removed(node.device));
        }
        joined.map(drop).map_err(DetectError::from)
    }

    /// Applies inotify events; an overflow reopens every node from a fresh scan.
    async fn apply(
        &mut self,
        events: Vec<(ReadFlags, String)>,
        overflow: bool,
    ) -> Result<(), DetectError> {
        if overflow {
            tracing::debug!("inotify queue overflowed, reopening hidraw nodes");
            let names: Vec<String> = self.nodes.keys().cloned().collect();
            for name in names {
                self.close(&name);
            }
            return self.rescan().await;
        }
        for (flags, name) in events {
            if flags.contains(ReadFlags::DELETE) {
                self.close(&name);
            } else if !self.nodes.contains_key(&name) {
                let sys_root = self.sys_root.clone();
                let lookup = name.clone();
                let Some(device) = self.scan(move || fido_device(&sys_root, &lookup)).await? else {
                    continue;
                };
                if let Some(device) = device {
                    self.open(&name, device);
                }
            }
        }
        Ok(())
    }

    /// Opens every FIDO node not watched yet; a scan that does not finish
    /// leaves the set of watched nodes unchanged.
    async fn rescan(&mut self) -> Result<(), DetectError> {
        self.rescan_at = None;
        let sys_root = self.sys_root.clone();
        let Some(devices) = self.scan(move || list_fido(&sys_root)).await? else {
            return Ok(());
        };
        for device in devices {
            if let Some(name) = device.id.0.strip_prefix("/dev/") {
                let name = name.to_owned();
                if !self.nodes.contains_key(&name) {
                    self.open(&name, device);
                }
            }
        }
        Ok(())
    }

    /// Runs `scan` on the blocking pool and returns its result, or `None`
    /// when another scan is in flight, it exceeds [`SCAN_TIMEOUT`], or the
    /// watcher is cancelled first.
    ///
    /// A scan skipped while another is in flight, or abandoned after
    /// [`SCAN_TIMEOUT`], schedules a full rescan after [`RESCAN_RETRY`].
    async fn scan<T: Send + 'static>(
        &mut self,
        scan: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Option<T>, DetectError> {
        if self.cancel.is_cancelled() {
            return Ok(None);
        }
        let permit = match Arc::clone(&self.scan_slot).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                self.busy_limit.log(now(), |suppressed| {
                    tracing::warn!(suppressed, "sysfs scan in flight; skipping it");
                });
                self.retry_later();
                return Ok(None);
            }
            Err(TryAcquireError::Closed) => return Err(DetectError::ScanSlotClosed),
        };
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            scan()
        });
        tokio::select! {
            () = self.cancel.cancelled() => Ok(None),
            joined = tokio::time::timeout(SCAN_TIMEOUT, task) => match joined {
                Ok(joined) => Ok(Some(joined?)),
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_ms = millis(SCAN_TIMEOUT),
                        "sysfs scan timed out; skipping it"
                    );
                    self.retry_later();
                    Ok(None)
                }
            }
        }
    }

    /// Schedules a full rescan after [`RESCAN_RETRY`] unless one is scheduled.
    fn retry_later(&mut self) {
        if self.rescan_at.is_none() {
            let now = now();
            self.rescan_at = Some(now.checked_add(RESCAN_RETRY).unwrap_or(now));
        }
    }

    fn open(&mut self, name: &str, device: Device) {
        if self.nodes.len() >= MAX_NODES {
            tracing::warn!(
                node = name,
                limit = MAX_NODES,
                "too many hidraw nodes, not watching"
            );
            return;
        }
        let fd = match (self.open)(&self.dev_root.join(name)) {
            Ok(Some(fd)) => fd,
            Ok(None) => {
                tracing::debug!(node = name, "not a character device, skipping");
                return;
            }
            // A new node is root-only until udev applies its access rules.
            Err(errno) => {
                tracing::debug!(
                    node = name,
                    error = &errno as &dyn std::error::Error,
                    "cannot open hidraw node"
                );
                return;
            }
        };
        let fd = match AsyncFd::new(fd) {
            Ok(fd) => fd,
            Err(error) => {
                tracing::debug!(
                    node = name,
                    error = &error as &dyn std::error::Error,
                    "cannot register hidraw node"
                );
                return;
            }
        };
        let cancel = self.nodes_cancel.child_token();
        let span = tracing::debug_span!("hidraw_node", node = name);
        let task = self
            .tasks
            .spawn(read_node(fd, device.clone(), self.tx.clone(), cancel.clone()).instrument(span))
            .id();
        tracing::debug!(node = name, "watching hidraw node");
        self.report(DeviceEvent::Added(device.clone()));
        self.nodes.insert(
            name.to_owned(),
            NodeHandle {
                task,
                cancel,
                device,
            },
        );
    }

    fn close(&mut self, name: &str) {
        if let Some(node) = self.nodes.remove(name) {
            node.cancel.cancel();
            tracing::debug!(node = name, "stopped watching hidraw node");
            self.report(DeviceEvent::Removed(node.device));
        }
    }
}

/// Reads queued inotify events until `EAGAIN` and returns the hidraw node
/// events and whether the queue overflowed.
fn read_inotify(inotify: &OwnedFd) -> Result<(Vec<(ReadFlags, String)>, bool), DetectError> {
    let mut buf = [MaybeUninit::<u8>::uninit(); INOTIFY_BUFFER_BYTES];
    let mut reader = inotify::Reader::new(inotify, &mut buf);
    let mut events = Vec::new();
    let mut overflow = false;
    loop {
        match reader.next() {
            Ok(event) => {
                if event.events().contains(ReadFlags::QUEUE_OVERFLOW) {
                    overflow = true;
                }
                let name = event
                    .file_name()
                    .and_then(|n| match n.to_str() {
                        Ok(name) => Some(name),
                        Err(error) => {
                            tracing::trace!(
                                error = &error as &dyn std::error::Error,
                                "non-UTF-8 device name ignored"
                            );
                            None
                        }
                    })
                    .filter(|n| is_node_name(n));
                if let Some(name) = name {
                    events.push((event.events(), name.to_owned()));
                }
            }
            Err(Errno::AGAIN) => return Ok((events, overflow)),
            Err(Errno::INTR) => {}
            Err(errno) => return Err(io("cannot read inotify events")(errno)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt as _;

    use rustix::fs::CWD;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use touchcue_core::ctaphid::{CTAPHID_CBOR, CTAPHID_KEEPALIVE, UPNEEDED};
    use touchcue_core::{DeviceId, DeviceKind, Outcome, Transport};

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Errno(#[from] Errno),
        #[error(transparent)]
        Detect(#[from] DetectError),
        #[error(transparent)]
        Join(#[from] JoinError),
        #[error("timed out")]
        Timeout(#[from] tokio::time::error::Elapsed),
        #[error(transparent)]
        Flags(#[from] std::num::TryFromIntError),
        #[error("{0}")]
        Unexpected(&'static str),
    }

    type TestResult = Result<(), TestError>;

    /// Longest wait for an expected event in tests on the real clock.
    const WAIT: Duration = Duration::from_secs(5);
    /// CTAPHID ping, which maps to no signal.
    const CTAPHID_PING: u8 = 0x81;

    fn report(cid: u32, cmd: u8, data: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; REPORT_BYTES];
        r[..4].copy_from_slice(&cid.to_be_bytes());
        r[4] = cmd;
        // The low two bytes of the length, big-endian.
        let len = data.len().to_be_bytes();
        r[5..7].copy_from_slice(&len[len.len() - 2..]);
        r[7..7 + data.len()].copy_from_slice(data);
        r
    }

    fn pending(cid: u32) -> Vec<u8> {
        report(cid, CTAPHID_KEEPALIVE, &[UPNEEDED])
    }

    fn device() -> Device {
        Device {
            id: DeviceId("/dev/hidraw5".to_owned()),
            kind: DeviceKind::Fido,
            transport: Transport::Usb,
            vid: Some(0x1050),
            pid: Some(0x0407),
            vendor: None,
            model: None,
            product: None,
        }
    }

    /// Returns a non-blocking pipe: the read end registered for a node task and the write end.
    fn pipe() -> Result<(AsyncFd<OwnedFd>, std::io::PipeWriter), TestError> {
        let (reader, writer) = std::io::pipe()?;
        let reader = OwnedFd::from(reader);
        rustix::fs::fcntl_setfl(&reader, OFlags::NONBLOCK)?;
        Ok((AsyncFd::new(reader)?, writer))
    }

    /// Opens a FIFO standing in for a hidraw node.
    fn open_fifo(path: &Path) -> rustix::io::Result<Option<OwnedFd>> {
        rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(Some)
    }

    /// Writes the sysfs entry of a FIDO hidraw node.
    fn sysfs_node(sys: &Path, name: &str) -> std::io::Result<()> {
        let dir = sys.join("class/hidraw").join(name).join("device");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("report_descriptor"),
            [0x06, 0xd0, 0xf1, 0x09, 0x01, 0xa1, 0x01, 0xc0],
        )?;
        fs::write(dir.join("uevent"), "HID_ID=0003:00001050:00000407\n")
    }

    async fn recv(rx: &mut mpsc::Receiver<Signal>) -> Result<Signal, TestError> {
        timeout(WAIT, rx.recv())
            .await?
            .ok_or(TestError::Unexpected("signal channel closed"))
    }

    #[test]
    fn frames_map_to_signal_kinds() {
        assert_eq!(
            kind_for(Frame::KeepaliveUpNeeded),
            Some(SignalKind::Pending {
                method: Method::Fido2,
                op: None
            })
        );
        assert_eq!(
            kind_for(Frame::U2fConditionsNotSatisfied),
            Some(SignalKind::Pending {
                method: Method::U2f,
                op: None
            })
        );
        assert_eq!(
            kind_for(Frame::KeepaliveProcessing),
            Some(SignalKind::Progress)
        );
        assert_eq!(
            kind_for(Frame::Done(Outcome::Touched)),
            Some(SignalKind::Resolved(Outcome::Touched))
        );
        assert_eq!(
            kind_for(Frame::Done(Outcome::Cancelled)),
            Some(SignalKind::Resolved(Outcome::Cancelled))
        );
        assert_eq!(kind_for(Frame::Other), None);
    }

    #[test]
    fn identical_signals_are_rate_limited() {
        let start = Instant::now();
        let kind = SignalKind::Progress;
        let last = Emitted {
            cid: 7,
            kind,
            at: start,
        };
        assert!(should_emit(None, 7, kind, start));
        assert!(!should_emit(Some(&last), 7, kind, start));
        assert!(!should_emit(
            Some(&last),
            7,
            kind,
            start + Duration::from_millis(249)
        ));
        assert!(should_emit(Some(&last), 7, kind, start + REPEAT_INTERVAL));
        assert!(should_emit(Some(&last), 8, kind, start));
        assert!(should_emit(
            Some(&last),
            7,
            SignalKind::Resolved(Outcome::Touched),
            start
        ));
    }

    #[test]
    fn bucket_bounds_varied_signals() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start);
        let mut last = None;
        let mut emitted = 0u32;
        for ms in 0..1000u32 {
            let now = start + Duration::from_millis(u64::from(ms));
            let kind = if ms % 2 == 0 {
                SignalKind::Progress
            } else {
                SignalKind::Resolved(Outcome::Failed)
            };
            if should_emit(last.as_ref(), ms, kind, now) && bucket.take(now) {
                last = Some(Emitted {
                    cid: ms,
                    kind,
                    at: now,
                });
                emitted += 1;
            }
        }
        assert!(
            (BUCKET_CAPACITY..=2 * BUCKET_CAPACITY).contains(&emitted),
            "{emitted}"
        );
    }

    #[test]
    fn bucket_refills_over_time() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start);
        for _ in 0..BUCKET_CAPACITY {
            assert!(bucket.take(start));
        }
        assert!(!bucket.take(start));
        assert!(!bucket.take(start + Duration::from_millis(49)));
        assert!(bucket.take(start + TOKEN_INTERVAL));
        assert!(!bucket.take(start + TOKEN_INTERVAL));
        let later = start + Duration::from_secs(10);
        for _ in 0..BUCKET_CAPACITY {
            assert!(bucket.take(later));
        }
        assert!(!bucket.take(later));
    }

    #[test]
    fn keepalives_pass_both_limits() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start);
        let mut last = None;
        let kind = SignalKind::Pending {
            method: Method::Fido2,
            op: None,
        };
        let mut emitted = 0u32;
        for step in 0..100u64 {
            let now = start + Duration::from_millis(step * 100);
            if should_emit(last.as_ref(), 1, kind, now) && bucket.take(now) {
                last = Some(Emitted {
                    cid: 1,
                    kind,
                    at: now,
                });
                emitted += 1;
            }
        }
        assert_eq!(emitted, 34);
    }

    #[test]
    fn only_character_devices_are_opened() -> TestResult {
        let dev = tempfile::tempdir()?;
        let file = dev.path().join("hidraw0");
        fs::write(&file, [0u8; 64])?;
        let link = dev.path().join("hidraw1");
        std::os::unix::fs::symlink("/dev/null", &link)?;

        assert!(open_node(&file)?.is_none());
        assert!(!matches!(open_node(&link), Ok(Some(_))));
        assert!(open_node(Path::new("/dev/null"))?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn node_reads_until_again_and_rearms() -> TestResult {
        let (fd, mut writer) = pipe()?;
        let (tx, mut rx) = mpsc::channel(SIGNAL_QUEUE);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(read_node(fd, device(), tx, cancel.clone()));
        writer.write_all(&[pending(1), report(1, CTAPHID_CBOR, &[0x00])].concat())?;
        assert!(matches!(
            recv(&mut rx).await?.kind,
            SignalKind::Pending { .. }
        ));
        assert_eq!(
            recv(&mut rx).await?.kind,
            SignalKind::Resolved(Outcome::Touched)
        );
        writer.write_all(&pending(2))?;
        assert_eq!(recv(&mut rx).await?.channel, Some(2));
        cancel.cancel();
        timeout(WAIT, task).await??;
        Ok(())
    }

    #[tokio::test]
    async fn node_continues_after_the_read_cap() -> TestResult {
        let (fd, mut writer) = pipe()?;
        let (tx, mut rx) = mpsc::channel(SIGNAL_QUEUE);
        let task = tokio::spawn(read_node(fd, device(), tx, CancellationToken::new()));
        let mut burst = Vec::new();
        for cid in 0..100 {
            burst.extend(report(cid, CTAPHID_PING, &[]));
        }
        burst.extend(pending(7));
        writer.write_all(&burst)?;
        assert_eq!(recv(&mut rx).await?.channel, Some(7));
        drop(writer);
        timeout(WAIT, task).await??;
        Ok(())
    }

    #[tokio::test]
    async fn node_ends_when_the_device_closes() -> TestResult {
        let (fd, writer) = pipe()?;
        let (tx, mut rx) = mpsc::channel(SIGNAL_QUEUE);
        let task = tokio::spawn(read_node(fd, device(), tx, CancellationToken::new()));
        drop(writer);
        timeout(WAIT, task).await??;
        assert!(rx.recv().await.is_none());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn node_rate_limit_follows_the_paused_clock() -> TestResult {
        let (fd, mut writer) = pipe()?;
        let (tx, mut rx) = mpsc::channel(SIGNAL_QUEUE);
        let task = tokio::spawn(read_node(fd, device(), tx, CancellationToken::new()));
        let burst: Vec<u8> = (0..25).flat_map(pending).collect();
        writer.write_all(&burst)?;
        for cid in 0..BUCKET_CAPACITY {
            assert_eq!(recv(&mut rx).await?.channel, Some(cid));
        }
        // The five reports over capacity are dropped; one token later the next passes.
        tokio::time::advance(TOKEN_INTERVAL).await;
        writer.write_all(&pending(999))?;
        assert_eq!(recv(&mut rx).await?.channel, Some(999));
        tokio::time::advance(Duration::from_secs(1)).await;
        let burst: Vec<u8> = (100..105).flat_map(pending).collect();
        writer.write_all(&burst)?;
        for cid in 100..105 {
            assert_eq!(recv(&mut rx).await?.channel, Some(cid));
        }
        drop(writer);
        timeout(WAIT, task).await??;
        Ok(())
    }

    #[tokio::test]
    async fn watcher_follows_created_and_deleted_nodes() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        sysfs_node(sys.path(), "hidraw5")?;
        let (tx, mut rx) = mpsc::channel(SIGNAL_QUEUE);
        let (devices_tx, mut devices) = mpsc::channel(DEVICE_QUEUE);
        let watcher = spawn_with(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            Some(devices_tx),
            &CancellationToken::new(),
            open_fifo,
        )?;
        let node = dev.path().join("hidraw5");
        rustix::fs::mkfifoat(CWD, &node, Mode::RUSR | Mode::WUSR)?;
        // The watcher opens the FIFO after the create event; until then writes have no reader.
        let writer = wait_for_reader(&node).await?;
        let added = timeout(WAIT, devices.recv()).await?;
        let id = DeviceId("/dev/hidraw5".to_owned());
        assert!(
            matches!(&added, Some(DeviceEvent::Added(device)) if device.id == id),
            "{added:?}"
        );
        rustix::io::write(&writer, &pending(3))?;
        let signal = recv(&mut rx).await?;
        assert_eq!(signal.device.id, id);
        assert_eq!(signal.channel, Some(3));

        fs::remove_file(&node)?;
        wait_for_no_reader(&writer).await?;
        let removed = timeout(WAIT, devices.recv()).await?;
        assert!(
            matches!(&removed, Some(DeviceEvent::Removed(device)) if device.id == id),
            "{removed:?}"
        );
        watcher.stop().await?;
        assert!(devices.recv().await.is_none(), "one removal per node");
        Ok(())
    }

    #[tokio::test]
    async fn overflow_reopens_nodes_from_a_fresh_scan() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        let (tx, _rx) = mpsc::channel(SIGNAL_QUEUE);
        let mut state = State::new(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            CancellationToken::new(),
            open_fifo,
        );
        for name in ["hidraw5", "hidraw6"] {
            sysfs_node(sys.path(), name)?;
            rustix::fs::mkfifoat(CWD, dev.path().join(name), Mode::RUSR | Mode::WUSR)?;
        }
        state.apply(Vec::new(), true).await?;
        assert_eq!(
            state.nodes.keys().collect::<Vec<_>>(),
            ["hidraw5", "hidraw6"]
        );

        let old = state.nodes.get("hidraw5").map(|node| node.cancel.clone());
        fs::remove_dir_all(sys.path().join("class/hidraw/hidraw6"))?;
        state.apply(Vec::new(), true).await?;
        assert_eq!(state.nodes.keys().collect::<Vec<_>>(), ["hidraw5"]);
        assert!(old.is_some_and(|token| token.is_cancelled()));

        state
            .apply(vec![(ReadFlags::DELETE, "hidraw5".to_owned())], false)
            .await?;
        assert!(state.nodes.is_empty());
        state
            .apply(vec![(ReadFlags::CREATE, "hidraw5".to_owned())], false)
            .await?;
        assert_eq!(state.nodes.keys().collect::<Vec<_>>(), ["hidraw5"]);
        state.nodes_cancel.cancel();
        while let Some(joined) = timeout(WAIT, state.tasks.join_next()).await? {
            joined?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn scan_in_flight_skips_and_schedules_a_rescan() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        sysfs_node(sys.path(), "hidraw5")?;
        rustix::fs::mkfifoat(CWD, dev.path().join("hidraw5"), Mode::RUSR | Mode::WUSR)?;
        let (tx, _rx) = mpsc::channel(SIGNAL_QUEUE);
        let mut state = State::new(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            CancellationToken::new(),
            open_fifo,
        );
        let in_flight = Arc::clone(&state.scan_slot)
            .try_acquire_owned()
            .map_err(|_none| TestError::Unexpected("scan slot taken"))?;
        state.rescan().await?;
        assert!(state.nodes.is_empty());
        assert!(state.rescan_at.is_some());

        drop(in_flight);
        state.rescan().await?;
        assert_eq!(state.nodes.keys().collect::<Vec<_>>(), ["hidraw5"]);
        assert!(state.rescan_at.is_none());
        state.nodes_cancel.cancel();
        while let Some(joined) = timeout(WAIT, state.tasks.join_next()).await? {
            joined?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn fake_nodes_are_skipped() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        for name in ["hidraw0", "hidraw1"] {
            sysfs_node(sys.path(), name)?;
        }
        fs::write(dev.path().join("hidraw0"), [0u8; 64])?;
        std::os::unix::fs::symlink("/dev/null", dev.path().join("hidraw1"))?;
        let (tx, _rx) = mpsc::channel(SIGNAL_QUEUE);
        let mut state = State::new(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            CancellationToken::new(),
            open_node,
        );
        state.rescan().await?;
        assert!(state.nodes.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn stop_returns_promptly() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        let (tx, _rx) = mpsc::channel(SIGNAL_QUEUE);
        let parent = CancellationToken::new();
        let watcher = spawn(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            None,
            &parent,
        )?;
        timeout(Duration::from_secs(1), watcher.stop()).await??;
        assert!(!parent.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn watcher_ends_when_the_receiver_is_dropped() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dev = tempfile::tempdir()?;
        let (tx, rx) = mpsc::channel(SIGNAL_QUEUE);
        let watcher = spawn(
            sys.path().to_owned(),
            dev.path().to_owned(),
            tx,
            None,
            &CancellationToken::new(),
        )?;
        drop(rx);
        timeout(WAIT, watcher.task).await???;
        Ok(())
    }

    #[tokio::test]
    async fn missing_dev_root_fails_to_spawn() -> TestResult {
        let dir = tempfile::tempdir()?;
        let (tx, _rx) = mpsc::channel(SIGNAL_QUEUE);
        let result = spawn(
            dir.path().to_owned(),
            dir.path().join("missing"),
            tx,
            None,
            &CancellationToken::new(),
        );
        assert!(matches!(result, Err(DetectError::Io { .. })));
        Ok(())
    }

    /// Opens the write end of the FIFO once the watcher holds it open.
    async fn wait_for_reader(path: &Path) -> Result<OwnedFd, TestError> {
        timeout(WAIT, async {
            loop {
                // Opening a FIFO for writing without a reader fails with ENXIO.
                let opened = fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(i32::try_from(OFlags::NONBLOCK.bits())?)
                    .open(path);
                match opened {
                    Ok(file) => return Ok(OwnedFd::from(file)),
                    Err(error) if error.raw_os_error() == Some(Errno::NXIO.raw_os_error()) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(TestError::from(error)),
                }
            }
        })
        .await?
    }

    /// Waits until no task holds the FIFO open for reading.
    async fn wait_for_no_reader(writer: &OwnedFd) -> TestResult {
        timeout(WAIT, async {
            loop {
                match rustix::io::write(writer.as_fd(), &report(0, CTAPHID_PING, &[])) {
                    Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(Errno::PIPE) => return Ok(()),
                    Err(errno) => return Err(TestError::from(errno)),
                }
            }
        })
        .await?
    }
}
