//! Linux implementations of `run`, `list-devices` and `trace`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt as _;
use tokio::signal::unix;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use touchcue_appinfo::linux::Resolver;
use touchcue_core::placeholders::device_values;
use touchcue_core::text::sanitize;
use touchcue_core::{
    Config, Device, Event, HookEvent, Machine, MachineConfig, Outcome, RateLimit, Signal,
    SignalKind,
};
use touchcue_detect::DetectError;
use touchcue_detect::linux::hidraw::DeviceEvent;
use touchcue_detect::linux::{hidraw, sysfs};
use touchcue_hooks::{HookSender, Hooks, HooksError, RequestEvents};
use touchcue_ipc::agent::{self, AgentPaths};
use touchcue_ipc::helper::{Helper, HelperConfig, HelperOutputs, Notice};
use touchcue_ipc::{Ipc, IpcConfig, IpcError, WireEvent};
use touchcue_ui::{Command, Ui, UiConfig, UiError};

use crate::check::backend_name;
use crate::daemon::{Attribute, Daemon, Sink, SystemAttribution};

/// sysfs mount point.
pub const SYS_ROOT: &str = "/sys";
/// Device node directory.
pub const DEV_ROOT: &str = "/dev";
/// Longest wait for the hidraw watcher and its node tasks to stop.
const DETECT_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Askpass notices queued for the daemon.
const NOTICE_QUEUE: usize = 16;
/// Longest wait for the helper socket to stop; it bounds itself to 2 s.
const HELPER_STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest device text printed, in chars.
const DEVICE_TEXT_MAX: usize = 128;
/// Shortest interval between two logged UI send failures.
const UI_WARN_INTERVAL: Duration = Duration::from_secs(10);
/// Interval at which IPC clients receive the full set of active requests, so
/// that a dropped event cannot leave them with stale state.
const RESYNC_INTERVAL: Duration = Duration::from_secs(5);
/// Longest wait for queued and running hook commands at shutdown before
/// they are ended.
const HOOKS_DRAIN: Duration = Duration::from_secs(2);

/// Failure of `run`, `list-devices` or `trace`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot register signal handlers")]
    Signals(#[source] io::Error),
    #[error("cannot start the UI")]
    UiStart(#[source] UiError),
    #[error("cannot shut down the UI")]
    UiShutdown(#[source] UiError),
    #[error("cannot start IPC")]
    IpcStart(#[source] IpcError),
    #[error("another touchcue instance is running")]
    AlreadyRunning(#[source] IpcError),
    #[error("cannot shut down IPC")]
    IpcShutdown(#[source] IpcError),
    #[error("cannot start the hidraw watcher")]
    WatcherStart(#[source] DetectError),
    #[error("hidraw watcher failed")]
    WatcherStop(#[source] DetectError),
    #[error("hidraw watcher did not stop within {DETECT_STOP_TIMEOUT:?}")]
    WatcherStopTimeout(#[source] tokio::time::error::Elapsed),
    #[error("hidraw watcher stopped unexpectedly")]
    WatcherGone,
    #[error("helper socket failed to stop")]
    HelperStop(#[source] IpcError),
    #[error("helper socket did not stop within {HELPER_STOP_TIMEOUT:?}")]
    HelperStopTimeout(#[source] tokio::time::error::Elapsed),
    #[error("hook commands did not stop")]
    HooksStop(#[source] HooksError),
    #[error("cannot write to stdout")]
    Stdout(#[source] io::Error),
}

/// Prints one line per FIDO device: `id vid:pid vendor product transport`.
///
/// # Errors
///
/// Returns [`Error::Stdout`] when stdout cannot be written.
#[tracing::instrument(skip_all, err)]
pub fn list_devices() -> Result<(), Error> {
    let mut out = io::stdout().lock();
    for device in sysfs::list_fido(Path::new(SYS_ROOT)) {
        writeln!(out, "{}", device_line(&device)).map_err(Error::Stdout)?;
    }
    Ok(())
}

/// Returns `id vid:pid vendor product transport`, with `-` for absent values
/// and device-supplied text sanitized.
pub fn device_line(device: &Device) -> String {
    let hex = |v: Option<u16>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:04x}"));
    let text = |v: Option<&str>| {
        v.and_then(|v| sanitize(v, DEVICE_TEXT_MAX))
            .unwrap_or_else(|| "-".to_owned())
    };
    format!(
        "{} {}:{} {} {} {}",
        device.id.0,
        hex(device.vid),
        hex(device.pid),
        text(device.vendor.as_deref()),
        text(device.product.as_deref()),
        device.transport.as_str(),
    )
}

/// Opens the device node `id` read-only and closes it again.
///
/// # Errors
///
/// Returns the error of opening the node.
pub fn probe_node(id: &str) -> io::Result<()> {
    File::open(id).map(drop)
}

/// SIGINT and SIGTERM streams; SIGHUP keeps its default action.
struct StopSignals {
    interrupt: unix::Signal,
    terminate: unix::Signal,
}

impl StopSignals {
    fn register() -> Result<Self, Error> {
        Ok(Self {
            interrupt: unix::signal(unix::SignalKind::interrupt()).map_err(Error::Signals)?,
            terminate: unix::signal(unix::SignalKind::terminate()).map_err(Error::Signals)?,
        })
    }

    /// Waits for either signal and returns its name; a closed stream counts as received.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// Returns the current time from the Tokio clock.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Runs the daemon until SIGINT or SIGTERM.
///
/// Hooks get `daemon_started` once everything started. Shutdown stops the
/// event loop and fires `daemon_stopping`, then stops detection and the
/// helper socket, then the hooks, ending commands still running after 2 s,
/// then the UI and IPC, each bounded by its own deadline. The helper socket
/// is optional: when it cannot start, gpg and ssh reports are not received
/// and the daemon runs on. A signal during startup abandons the pending
/// start, shuts down what already started and returns `Ok(())`.
///
/// # Errors
///
/// Returns the error of the UI or detector when it fails to start, or, after
/// shutting down, the first error of the event loop, the watcher, the hooks,
/// the UI and IPC in that order. Later errors are logged. Returns
/// [`Error::AlreadyRunning`] when another touchcue serves IPC; any other IPC
/// failure is logged and the daemon runs without IPC.
#[tracing::instrument(skip_all, fields(backend = tracing::field::Empty), err)]
pub async fn run(config: Config) -> Result<(), Error> {
    let mut signals = StopSignals::register()?;
    let root = CancellationToken::new();
    let fido = &config.sources.fido;
    let machine = Machine::new(MachineConfig {
        keepalive_timeout: Duration::from_millis(fido.keepalive_timeout_ms),
        retry_window: Duration::from_millis(fido.retry_window_ms),
    });
    let hooks = Hooks::spawn(config.hooks.clone());
    let ui = Ui::spawn(
        UiConfig {
            output: config.output.clone(),
            popup: config.popup.clone(),
            notification: config.notification.clone(),
        },
        root.child_token(),
    );
    let ui = tokio::select! {
        name = signals.recv() => {
            tracing::info!(signal = name, "stopping during startup");
            root.cancel();
            return Ok(());
        }
        ui = ui => ui.map_err(Error::UiStart)?,
    };
    let backend = backend_name(ui.backend());
    tracing::Span::current().record("backend", backend);
    let ipc = tokio::select! {
        name = signals.recv() => {
            tracing::info!(signal = name, "stopping during startup");
            return finish(None, Outputs::new(ui, None, hooks.sender()), hooks, &root).await;
        }
        ipc = spawn_ipc(&config, &root) => ipc,
    };
    let ipc = match ipc {
        Ok(ipc) => ipc,
        Err(Error::IpcStart(error @ IpcError::AlreadyRunning { .. })) => {
            return finish(
                Some(Error::AlreadyRunning(error)),
                Outputs::new(ui, None, hooks.sender()),
                hooks,
                &root,
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "continuing without IPC"
            );
            None
        }
    };
    let mut outputs = Outputs::new(ui, ipc, hooks.sender());
    let (tx, rx) = mpsc::channel(hidraw::SIGNAL_QUEUE);
    let (notices_tx, notices) = mpsc::channel(NOTICE_QUEUE);
    let (devices_tx, devices) = mpsc::channel(hidraw::DEVICE_QUEUE);
    let watcher = match spawn_watcher(&config, tx.clone(), devices_tx, &root) {
        Ok(watcher) => watcher,
        Err(error) => return finish(Some(error), outputs, hooks, &root).await,
    };
    let helper_outputs = HelperOutputs {
        signals: tx.clone(),
        notices: notices_tx,
    };
    let (helper, agent) = tokio::select! {
        name = signals.recv() => {
            tracing::info!(signal = name, "stopping during startup");
            let mut first = None;
            stop_detection(&mut first, watcher, None).await;
            return finish(first, outputs, hooks, &root).await;
        }
        started = spawn_helper(&config, helper_outputs, &root) => started,
    };
    // Held while no detector runs so that the channel stays open; otherwise
    // the channel closes once every running detector has stopped.
    let idle_tx = (watcher.is_none() && helper.is_none()).then_some(tx);
    tracing::info!(
        backend,
        ipc = outputs.ipc.is_some(),
        helper = helper.is_some(),
        "touchcue running"
    );
    let sender = hooks.sender();
    sender.fire(HookEvent::DaemonStarted, &BTreeMap::new());

    let attribution = SystemAttribution::new(
        Resolver::system().with_skip(config.requester.skip_list()),
        agent,
    );
    let mut daemon = Daemon::new(machine, config, attribution, outputs);
    let ended = event_loop(&mut daemon, rx, notices, devices, &sender, &mut signals).await;
    sender.fire(HookEvent::DaemonStopping, &BTreeMap::new());
    drop(idle_tx);
    let mut first = None;
    if let Err(error) = ended {
        first = Some(error);
    }
    stop_detection(&mut first, watcher, helper).await;
    outputs = daemon.into_sink();
    finish(first, outputs, hooks, &root).await
}

/// Starts the hidraw watcher when `sources.fido` is enabled.
fn spawn_watcher(
    config: &Config,
    tx: mpsc::Sender<Signal>,
    devices: mpsc::Sender<DeviceEvent>,
    root: &CancellationToken,
) -> Result<Option<hidraw::Watcher>, Error> {
    if !config.sources.fido.enabled {
        tracing::info!("FIDO detection is disabled");
        return Ok(None);
    }
    hidraw::spawn(
        PathBuf::from(SYS_ROOT),
        PathBuf::from(DEV_ROOT),
        tx,
        Some(devices),
        root,
    )
    .map(Some)
    .map_err(Error::WatcherStart)
}

/// Stops the hidraw watcher and the helper socket, each within its deadline,
/// keeping the first failure in `first`.
async fn stop_detection(
    first: &mut Option<Error>,
    watcher: Option<hidraw::Watcher>,
    helper: Option<Helper>,
) {
    if let Some(watcher) = watcher {
        match tokio::time::timeout(DETECT_STOP_TIMEOUT, watcher.stop()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => keep_first(first, Error::WatcherStop(error)),
            Err(elapsed) => keep_first(first, Error::WatcherStopTimeout(elapsed)),
        }
    }
    if let Some(helper) = helper {
        match tokio::time::timeout(HELPER_STOP_TIMEOUT, helper.stop()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => keep_first(first, Error::HelperStop(error)),
            Err(elapsed) => keep_first(first, Error::HelperStopTimeout(elapsed)),
        }
    }
}

/// Looks up gpg-agent's sockets and starts the helper socket when
/// `sources.gpg` is enabled, returning each when available; a failure is
/// logged and leaves it `None`.
#[tracing::instrument(skip_all)]
async fn spawn_helper(
    config: &Config,
    outputs: HelperOutputs,
    root: &CancellationToken,
) -> (Option<Helper>, Option<AgentPaths>) {
    if !config.sources.gpg.enabled {
        tracing::info!("gpg and ssh reports are disabled");
        return (None, None);
    }
    let agent = match agent::paths().await {
        Ok(paths) => Some(paths),
        Err(error) => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "gpg-agent not found; gpg reports are not filtered by UIF or attributed"
            );
            None
        }
    };
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        tracing::warn!("XDG_RUNTIME_DIR is unset; gpg and ssh reports are disabled");
        return (None, agent);
    };
    let keepalive = Duration::from_millis(config.sources.fido.keepalive_timeout_ms / 3);
    let cfg = HelperConfig {
        runtime_dir: PathBuf::from(runtime_dir),
        agent: agent.clone(),
        keepalive,
    };
    match Helper::spawn(cfg, outputs, root).await {
        Ok(helper) => (Some(helper), agent),
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "helper socket not started; gpg and ssh reports are disabled"
            );
            (None, agent)
        }
    }
}

/// Stops the hooks, ending commands still running after [`HOOKS_DRAIN`],
/// shuts the outputs down, cancels every remaining task and returns
/// `first`, else the first shutdown error.
async fn finish(
    first: Option<Error>,
    outputs: Outputs,
    hooks: Hooks,
    root: &CancellationToken,
) -> Result<(), Error> {
    let mut first = first;
    if let Err(error) = hooks.shutdown(HOOKS_DRAIN).await {
        keep_first(&mut first, Error::HooksStop(error));
    }
    if let Err(error) = shutdown(outputs).await {
        keep_first(&mut first, error);
    }
    root.cancel();
    first.map_or(Ok(()), Err)
}

/// Stores `error` in `first` if it is empty, else logs it, so that every
/// error is reported exactly once.
fn keep_first(first: &mut Option<Error>, error: Error) {
    if first.is_some() {
        tracing::error!(
            error = &error as &dyn std::error::Error,
            "further failure while stopping"
        );
    } else {
        *first = Some(error);
    }
}

/// Starts IPC as configured, or returns `None` when every IPC output is off or
/// `XDG_RUNTIME_DIR` is unset.
#[tracing::instrument(skip_all, fields(path = tracing::field::Empty))]
async fn spawn_ipc(config: &Config, root: &CancellationToken) -> Result<Option<Ipc>, Error> {
    let json = config.ipc.enabled;
    let dbus = config.dbus.enabled;
    let compat_maxbaz = config.compat.maxbaz_socket.enabled;
    if !(json || dbus || compat_maxbaz) {
        tracing::info!("IPC is disabled");
        return Ok(None);
    }
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        tracing::warn!("XDG_RUNTIME_DIR is unset; IPC is disabled");
        return Ok(None);
    };
    tracing::Span::current().record(
        "path",
        tracing::field::display(Path::new(&runtime_dir).display()),
    );
    let ipc = Ipc::spawn(
        IpcConfig {
            runtime_dir: PathBuf::from(runtime_dir),
            json,
            dbus,
            compat_maxbaz,
        },
        root.child_token(),
    )
    .await
    .map_err(Error::IpcStart)?;
    let endpoints = ipc.endpoints();
    tracing::info!(
        json = endpoints.json,
        dbus = endpoints.dbus,
        compat = endpoints.compat,
        "IPC started"
    );
    Ok(Some(ipc))
}

/// Feeds signals, askpass notices and deadlines to the daemon, and device
/// events to the hooks, until SIGINT or SIGTERM. A closed notice channel
/// means the helper socket stopped, and a closed device channel that the
/// hidraw watcher is not running; the daemon runs on without them.
///
/// # Errors
///
/// Returns [`Error::WatcherGone`] when the signal channel closes.
async fn event_loop<A: Attribute, S: Sink>(
    daemon: &mut Daemon<A, S>,
    mut rx: mpsc::Receiver<Signal>,
    mut notices: mpsc::Receiver<Notice>,
    mut devices: mpsc::Receiver<DeviceEvent>,
    hooks: &HookSender,
    signals: &mut StopSignals,
) -> Result<(), Error> {
    let mut notices_open = true;
    // Already closed and empty when FIDO detection is disabled.
    let mut devices_open = !(devices.is_closed() && devices.is_empty());
    let mut resync = tokio::time::interval_at(
        tokio::time::Instant::now() + RESYNC_INTERVAL,
        RESYNC_INTERVAL,
    );
    resync.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let deadline = daemon.next_deadline();
        let wake = tokio::time::Instant::from_std(deadline.unwrap_or_else(now));
        tokio::select! {
            name = signals.recv() => {
                tracing::info!(signal = name, "stopping");
                return Ok(());
            }
            signal = rx.recv() => match signal {
                Some(signal) => daemon.signal(signal, now()).await,
                None => return Err(Error::WatcherGone),
            },
            notice = notices.recv(), if notices_open => {
                if let Some(notice) = notice {
                    daemon.notice(notice, now()).await;
                } else {
                    tracing::debug!("askpass notices stopped");
                    notices_open = false;
                }
            }
            device = devices.recv(), if devices_open => match device {
                Some(DeviceEvent::Added(device)) => {
                    hooks.fire(HookEvent::DeviceAdded, &device_values(&device));
                }
                Some(DeviceEvent::Removed(device)) => {
                    hooks.fire(HookEvent::DeviceRemoved, &device_values(&device));
                }
                None => {
                    tracing::debug!("device events stopped");
                    devices_open = false;
                }
            },
            () = tokio::time::sleep_until(wake), if deadline.is_some() => {}
            _ = resync.tick() => daemon.resync(),
        }
        daemon.tick(now()).await;
    }
}

/// The real UI, IPC and hook outputs.
struct Outputs {
    ui: Ui,
    backend: &'static str,
    ipc: Option<Ipc>,
    ui_limit: RateLimit,
    hooks: HookSender,
    requests: RequestEvents,
}

impl Outputs {
    fn new(ui: Ui, ipc: Option<Ipc>, hooks: HookSender) -> Self {
        let backend = backend_name(ui.backend());
        Self {
            ui,
            backend,
            ipc,
            ui_limit: RateLimit::new(UI_WARN_INTERVAL),
            hooks,
            requests: RequestEvents::default(),
        }
    }
}

impl Sink for Outputs {
    async fn ui(&mut self, command: Command) -> bool {
        let (action, id) = match &command {
            Command::Show(prompt) => ("show", prompt.id),
            Command::Update(prompt) => ("update", prompt.id),
            Command::Hide(id) => ("hide", *id),
        };
        let error = match self.ui.send(command).await {
            Ok(()) => return true,
            Err(error) => error,
        };
        let backend = self.backend;
        self.ui_limit.log(now(), |suppressed| {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                id = %id,
                action,
                backend,
                suppressed,
                "cannot send command to UI"
            );
        });
        false
    }

    fn publish(&mut self, event: &Event, values: &BTreeMap<String, String>) {
        let wire = WireEvent::new(event, values);
        let (Event::Started(request) | Event::Updated(request) | Event::Ended { request, .. }) =
            event;
        let elapsed = now().saturating_duration_since(request.started);
        let elapsed_ms = elapsed
            .as_secs()
            .saturating_mul(1000)
            .saturating_add(u64::from(elapsed.subsec_millis()));
        tracing::info!(
            id = wire.id,
            device = %request.device.id.0,
            elapsed_ms,
            kind = ?wire.kind,
            source = %wire.source,
            state = %wire.state,
            reason = wire.reason.as_deref(),
            backend = self.backend,
            "request event"
        );
        if let Some(ipc) = &self.ipc {
            ipc.publish(&wire);
        }
        for hook_event in self.requests.map(event) {
            self.hooks.fire(hook_event, values);
        }
    }

    fn resync(&mut self, active: &[(Event, BTreeMap<String, String>)]) {
        if let Some(ipc) = &self.ipc {
            let wire: Vec<WireEvent> = active
                .iter()
                .map(|(event, values)| WireEvent::new(event, values))
                .collect();
            ipc.resync(&wire);
        }
    }
}

/// Shuts down the UI and IPC and returns the first failure; a later one is logged.
#[tracing::instrument(skip_all)]
async fn shutdown(outputs: Outputs) -> Result<(), Error> {
    let mut first = None;
    let deadline = tokio::time::Instant::now() + touchcue_ui::SHUTDOWN_TIMEOUT;
    if let Err(error) = outputs.ui.shutdown(deadline).await {
        keep_first(&mut first, Error::UiShutdown(error));
    }
    if let Some(ipc) = outputs.ipc
        && let Err(error) = ipc.shutdown().await
    {
        keep_first(&mut first, Error::IpcShutdown(error));
    }
    first.map_or(Ok(()), Err)
}

/// Prints each detection signal until SIGINT or SIGTERM.
///
/// Runs regardless of `sources.fido.enabled`. A signal also interrupts a
/// pending stdout write, which may leave a partial line.
///
/// # Errors
///
/// Returns an error when the watcher cannot start, stops on its own or fails
/// to stop, or stdout cannot be written; the watcher is stopped in every case.
#[tracing::instrument(skip_all, err)]
pub async fn trace() -> Result<(), Error> {
    let mut signals = StopSignals::register()?;
    let root = CancellationToken::new();
    let (tx, mut rx) = mpsc::channel(hidraw::SIGNAL_QUEUE);
    let watcher = hidraw::spawn(
        PathBuf::from(SYS_ROOT),
        PathBuf::from(DEV_ROOT),
        tx,
        None,
        &root,
    )
    .map_err(Error::WatcherStart)?;
    let start = now();
    let mut out = tokio::io::stdout();
    let result = loop {
        tokio::select! {
            _ = signals.recv() => break Ok(()),
            signal = rx.recv() => {
                let Some(signal) = signal else {
                    break Err(Error::WatcherGone);
                };
                let mut line = trace_line(now().saturating_duration_since(start), &signal);
                line.push('\n');
                let written = async {
                    out.write_all(line.as_bytes()).await?;
                    out.flush().await
                };
                tokio::select! {
                    _ = signals.recv() => break Ok(()),
                    written = written => if let Err(error) = written {
                        break Err(Error::Stdout(error));
                    },
                }
            }
        }
    };
    let mut first = None;
    if let Err(error) = result {
        keep_first(&mut first, error);
    }
    match tokio::time::timeout(DETECT_STOP_TIMEOUT, watcher.stop()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => keep_first(&mut first, Error::WatcherStop(error)),
        Err(elapsed) => keep_first(&mut first, Error::WatcherStopTimeout(elapsed)),
    }
    root.cancel();
    first.map_or(Ok(()), Err)
}

/// Formats a signal as `elapsed_ms device channel kind`, never including payload bytes.
///
/// `channel` is eight hex digits or `-`; `kind` is `pending <method> [op]`,
/// `progress` or `resolved <outcome>`.
pub fn trace_line(elapsed: Duration, signal: &Signal) -> String {
    let channel = signal
        .channel
        .map_or_else(|| "-".to_owned(), |channel| format!("{channel:08x}"));
    let kind = match signal.kind {
        SignalKind::Pending { method, op: None } => format!("pending {}", method.as_str()),
        SignalKind::Pending {
            method,
            op: Some(op),
        } => format!("pending {} {}", method.as_str(), op.as_str()),
        SignalKind::Progress => "progress".to_owned(),
        SignalKind::Resolved(outcome) => format!(
            "resolved {}",
            match outcome {
                Outcome::Touched => "touched",
                Outcome::Cancelled => "cancelled",
                Outcome::Failed => "failed",
                Outcome::TimedOut => "timed_out",
            }
        ),
    };
    format!(
        "{} {} {channel} {kind}",
        elapsed.as_millis(),
        signal.device.id.0
    )
}

#[cfg(test)]
mod tests {
    use touchcue_core::{DeviceId, DeviceKind, Method, Op, SignalClass, Source, Transport};

    use super::*;

    fn device() -> Device {
        Device {
            id: DeviceId("/dev/hidraw3".to_owned()),
            kind: DeviceKind::Fido,
            transport: Transport::Usb,
            vid: Some(0x1050),
            pid: Some(0x0407),
            vendor: Some("Yubico".to_owned()),
            model: None,
            product: Some("YubiKey\u{1b}[31m OTP+FIDO".to_owned()),
        }
    }

    fn signal(kind: SignalKind, channel: Option<u32>) -> Signal {
        Signal {
            device: device(),
            source: Source::Fido,
            class: SignalClass::Asserted,
            kind,
            channel,
            pids: Vec::new(),
        }
    }

    #[test]
    fn trace_lines_name_kind_without_payload() {
        let ms = Duration::from_millis(1234);
        let pending = SignalKind::Pending {
            method: Method::Fido2,
            op: None,
        };
        assert_eq!(
            trace_line(ms, &signal(pending, Some(0xab))),
            "1234 /dev/hidraw3 000000ab pending fido2"
        );
        let register = SignalKind::Pending {
            method: Method::U2f,
            op: Some(Op::Register),
        };
        assert_eq!(
            trace_line(ms, &signal(register, None)),
            "1234 /dev/hidraw3 - pending u2f register"
        );
        assert_eq!(
            trace_line(ms, &signal(SignalKind::Progress, Some(1))),
            "1234 /dev/hidraw3 00000001 progress"
        );
        assert_eq!(
            trace_line(
                ms,
                &signal(SignalKind::Resolved(Outcome::Cancelled), Some(1))
            ),
            "1234 /dev/hidraw3 00000001 resolved cancelled"
        );
    }

    #[test]
    fn device_line_sanitizes_text() {
        assert_eq!(
            device_line(&device()),
            "/dev/hidraw3 1050:0407 Yubico YubiKey [31m OTP+FIDO usb"
        );
    }
}
