//! UI task showing Wayland popups, with session-bus notifications on a
//! second task.

mod icon;
mod modal;
mod notify;
mod output;
mod placement;
mod render;
mod wayland;
mod x11;

use std::future;
use std::time::Duration;

use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use touchcue_core::config::OutputMode;
use touchcue_core::{RequestId, RequestState};
use tracing::{Instrument, debug, info, info_span, instrument, warn};

use self::modal::ModalTimer;
use self::notify::{Notifier, NotifyWorker};
use self::output::{Incoming, OutputError, PopupOutput};
use self::placement::Placement;
use crate::text::sanitize_command;
use crate::timing::{Action, Timing};
use crate::{
    Backend, Capabilities, Command, HIDE_SEND_TIMEOUT, INIT_TIMEOUT, Prompt, QUEUE_LEN,
    SHUTDOWN_TIMEOUT, UiConfig, UiError,
};

/// Time reserved for the UI task to exit after its notifier task.
const EXIT_MARGIN: Duration = Duration::from_millis(200);
/// Interval within which a repeating warning is logged once.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum Msg {
    Cmd(Command),
    /// Stops the UI task, which must have exited by `deadline`.
    Shutdown {
        deadline: Instant,
    },
}

/// Sending side of the UI task.
#[derive(Debug)]
pub(crate) struct Handle {
    sender: mpsc::Sender<Msg>,
    task: JoinHandle<()>,
}

impl Handle {
    pub(crate) async fn send(&self, cmd: Command) -> Result<(), UiError> {
        let (id, kind) = describe(&cmd);
        if let Command::Hide(_) = cmd {
            return match tokio::time::timeout(HIDE_SEND_TIMEOUT, self.sender.send(Msg::Cmd(cmd)))
                .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_unsent)) => {
                    debug!(%id, kind, "UI task is not running; command dropped");
                    Err(UiError::Disconnected)
                }
                Err(source) => Err(UiError::SendTimeout {
                    timeout: HIDE_SEND_TIMEOUT,
                    source,
                }),
            };
        }
        self.sender
            .try_send(Msg::Cmd(cmd))
            .map_err(|err| match err {
                TrySendError::Full(_) => UiError::QueueFull,
                TrySendError::Closed(_) => {
                    debug!(%id, kind, "UI task is not running; command dropped");
                    UiError::Disconnected
                }
            })
    }

    #[instrument(skip_all, err)]
    pub(crate) async fn shutdown(mut self, deadline: Instant) -> Result<(), UiError> {
        let stop = Msg::Shutdown { deadline };
        match tokio::time::timeout_at(deadline, self.sender.send(stop)).await {
            Ok(Ok(())) => {}
            Ok(Err(_unsent)) => debug!("UI task already exited"),
            Err(_elapsed) => debug!("UI queue stayed full until the deadline"),
        }
        match tokio::time::timeout_at(deadline, &mut self.task).await {
            Ok(Ok(())) => {
                info!("UI task stopped");
                Ok(())
            }
            Ok(Err(err)) => Err(UiError::Task(err)),
            Err(elapsed) => {
                self.task.abort();
                Err(UiError::ShutdownTimeout(elapsed))
            }
        }
    }
}

pub(crate) async fn spawn(
    cfg: UiConfig,
    cancel: CancellationToken,
) -> Result<(Backend, Option<Handle>), UiError> {
    let outputs = tokio::time::timeout(INIT_TIMEOUT, Outputs::open(&cfg))
        .await
        .map_err(|source| UiError::InitTimeout {
            timeout: INIT_TIMEOUT,
            source,
        })?;
    if outputs.backend == Backend::Off {
        return Ok((Backend::Off, None));
    }
    let (sender, queue) = mpsc::channel(QUEUE_LEN);
    let app = App {
        timing: Timing::new(
            Duration::from_millis(cfg.popup.show_delay_ms),
            Duration::from_millis(cfg.popup.min_display_ms),
        ),
        popups: outputs.popups,
        modal: cfg.popup.modal,
        modal_timer: ModalTimer::default(),
        notifier: outputs.notifier.map(NotifyWorker::start),
    };
    info!(
        backend = ?outputs.backend,
        notifications = app.notifier.is_some(),
        "UI task started"
    );
    let task = tokio::spawn(run(app, queue, cancel).instrument(info_span!("ui")));
    Ok((outputs.backend, Some(Handle { sender, task })))
}

#[instrument(skip_all)]
pub(crate) async fn probe() -> Capabilities {
    let wayland = tokio::time::timeout(
        INIT_TIMEOUT,
        tokio::task::spawn_blocking(|| {
            wayland::connect()
                .map(|(_conn, globals, queue)| wayland::has_layer_shell(&globals, &queue))
        }),
    )
    .await;
    let (wayland, layer_shell) = match wayland {
        Ok(Ok(Ok(layer_shell))) => (true, layer_shell),
        Ok(Ok(Err(err))) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "Wayland unavailable"
            );
            (false, false)
        }
        Ok(Err(err)) => {
            warn!(
                error = &err as &dyn std::error::Error,
                "Wayland probe failed"
            );
            (false, false)
        }
        Err(_) => {
            debug!(
                timeout_ms = INIT_TIMEOUT.as_millis(),
                "Wayland probe timed out"
            );
            (false, false)
        }
    };
    let x11 = tokio::time::timeout(INIT_TIMEOUT, tokio::task::spawn_blocking(x11::connect)).await;
    let x11 = match x11 {
        Ok(Ok(Ok(_))) => true,
        Ok(Ok(Err(err))) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "X11 popups unavailable"
            );
            false
        }
        Ok(Err(err)) => {
            warn!(error = &err as &dyn std::error::Error, "X11 probe failed");
            false
        }
        Err(_) => {
            debug!(timeout_ms = INIT_TIMEOUT.as_millis(), "X11 probe timed out");
            false
        }
    };
    let notifications = tokio::time::timeout(INIT_TIMEOUT, async {
        let conn = notify::session().await?;
        notify::service_available(&conn).await
    })
    .await;
    let notifications = match notifications {
        Ok(Ok(available)) => available,
        Ok(Err(err)) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "session bus unavailable"
            );
            false
        }
        Err(_) => {
            debug!(
                timeout_ms = INIT_TIMEOUT.as_millis(),
                "session bus probe timed out"
            );
            false
        }
    };
    let caps = Capabilities {
        wayland,
        layer_shell,
        x11,
        notifications,
    };
    debug!(?caps, "probed desktop capabilities");
    caps
}

/// State of the UI task.
struct App {
    timing: Timing,
    popups: Option<PopupOutput>,
    /// Popups of waiting requests hold overlays that block clicks.
    modal: bool,
    modal_timer: ModalTimer,
    notifier: Option<NotifyWorker>,
}

impl App {
    async fn on_command(&mut self, cmd: Command) {
        let (id, kind) = describe(&cmd);
        debug!(%id, kind, "command received");
        let cmd = sanitize_command(cmd);
        let actions = self.timing.command(cmd, Instant::now().into_std());
        self.apply(actions).await;
    }

    async fn on_tick(&mut self) {
        let now = Instant::now().into_std();
        let actions = self.timing.tick(now);
        self.apply(actions).await;
        if let Some(output) = &mut self.popups
            && let Err(err) = output.expire_overlays(now)
        {
            self.lose_popups(&err);
        }
    }

    /// Returns the earliest time [`App::on_tick`] has work.
    fn next_deadline(&self) -> Option<std::time::Instant> {
        let overlays = self
            .popups
            .as_ref()
            .and_then(PopupOutput::next_overlay_deadline);
        match (self.timing.next_deadline(), overlays) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Reports whether the prompt's popups may hold overlays.
    fn modal(&mut self, prompt: &Prompt) -> bool {
        self.modal
            && self.popups.is_some()
            && self
                .modal_timer
                .sync(prompt.id, prompt.state == RequestState::Waiting)
    }

    async fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Show(prompt) => {
                    let modal = self.modal(&prompt);
                    if let Some(output) = &mut self.popups
                        && let Err(err) = output.show(&prompt, modal).await
                    {
                        self.lose_popups(&err);
                    }
                    if let Some(notifier) = &mut self.notifier {
                        notifier.show(&prompt);
                    }
                }
                Action::Update(prompt) => {
                    let modal = self.modal(&prompt);
                    if let Some(output) = &mut self.popups
                        && let Err(err) = output.update(&prompt, modal).await
                    {
                        self.lose_popups(&err);
                    }
                    if let Some(notifier) = &mut self.notifier {
                        notifier.show(&prompt);
                    }
                }
                Action::Hide(id) => {
                    self.modal_timer.remove(id);
                    if let Some(output) = &mut self.popups
                        && let Err(err) = output.hide(id)
                    {
                        self.lose_popups(&err);
                    }
                    if let Some(notifier) = &mut self.notifier {
                        notifier.hide(id);
                    }
                }
            }
        }
    }

    /// Handles display events and sends pending requests; a failure
    /// disables popups.
    async fn dispatch(&mut self) {
        let Some(output) = &mut self.popups else {
            return;
        };
        if let Err(err) = output.dispatch().await {
            self.lose_popups(&err);
            return;
        }
        self.take_dismissed();
    }

    /// Ends the modal state of requests whose popups a click dismissed.
    fn take_dismissed(&mut self) {
        let Some(output) = &mut self.popups else {
            return;
        };
        for id in output.take_dismissed() {
            self.modal_timer.dismiss(id);
        }
    }

    fn on_incoming(&mut self, incoming: Result<Incoming, OutputError>) {
        let Some(output) = &mut self.popups else {
            return;
        };
        if let Err(err) = incoming.and_then(|incoming| output.handle(incoming)) {
            self.lose_popups(&err);
            return;
        }
        self.take_dismissed();
    }

    /// Drops the display connection, which destroys every popup and overlay.
    fn lose_popups(&mut self, err: &OutputError) {
        warn!(
            error = err as &dyn std::error::Error,
            "display connection lost; popups are disabled"
        );
        self.popups = None;
        self.modal_timer.clear();
    }
}

/// Waits until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(Instant::from_std(deadline)).await,
        None => future::pending().await,
    }
}

/// Waits until the display connection has been read, or forever without popups.
async fn read_display(popups: Option<&PopupOutput>) -> Result<Incoming, OutputError> {
    match popups {
        Some(output) => output.read().await,
        None => future::pending().await,
    }
}

/// Body of the UI task: applies commands and timers until stopped, then
/// withdraws every prompt.
async fn run(mut app: App, mut queue: mpsc::Receiver<Msg>, cancel: CancellationToken) {
    let deadline = loop {
        app.dispatch().await;
        let next = app.next_deadline();
        tokio::select! {
            () = cancel.cancelled() => break Instant::now() + SHUTDOWN_TIMEOUT,
            msg = queue.recv() => match msg {
                Some(Msg::Cmd(cmd)) => app.on_command(cmd).await,
                Some(Msg::Shutdown { deadline }) => break deadline,
                None => break Instant::now() + SHUTDOWN_TIMEOUT,
            },
            () = sleep_until(next) => app.on_tick().await,
            incoming = read_display(app.popups.as_ref()) => app.on_incoming(incoming),
        }
    };
    if let Some(mut output) = app.popups.take() {
        let hidden = match output.hide_all() {
            Ok(()) => output.dispatch().await,
            Err(err) => Err(err),
        };
        if let Err(err) = hidden {
            debug!(
                error = &err as &dyn std::error::Error,
                "failed to flush popup removal"
            );
        }
    }
    if let Some(notifier) = app.notifier.take() {
        let notifier_deadline = deadline.checked_sub(EXIT_MARGIN).unwrap_or(deadline);
        notifier.shutdown(notifier_deadline).await;
    }
}

/// Returns the request id and kind of a command for logging.
fn describe(cmd: &Command) -> (RequestId, &'static str) {
    match cmd {
        Command::Show(prompt) => (prompt.id, "show"),
        Command::Update(prompt) => (prompt.id, "update"),
        Command::Hide(id) => (*id, "hide"),
    }
}

/// Outputs opened for the configured mode or its fallback.
struct Outputs {
    backend: Backend,
    popups: Option<PopupOutput>,
    notifier: Option<Notifier>,
}

impl Outputs {
    /// Opens the outputs the configured modes may use and keeps those of
    /// the mode [`crate::choose`] selects.
    async fn open(cfg: &UiConfig) -> Self {
        // Modes after `none` are never consulted.
        let modes: Vec<_> = [cfg.output.mode, cfg.output.fallback]
            .into_iter()
            .scan(false, |stopped, mode| {
                let consulted = !*stopped;
                *stopped |= mode == OutputMode::Off;
                consulted.then_some(mode)
            })
            .collect();
        let wants = |wanted: &[OutputMode]| modes.iter().any(|mode| wanted.contains(mode));
        let popups = if wants(&[OutputMode::Popup, OutputMode::Both]) {
            PopupOutput::open(&Placement::new(&cfg.popup)).await
        } else {
            None
        };
        let notifier = if wants(&[OutputMode::Notification, OutputMode::Both]) {
            open_notifier(cfg).await
        } else {
            None
        };
        let caps = Capabilities {
            wayland: matches!(popups, Some(PopupOutput::Wayland { .. })),
            layer_shell: matches!(popups, Some(PopupOutput::Wayland { .. })),
            x11: matches!(popups, Some(PopupOutput::X11(_))),
            notifications: notifier.is_some(),
        };
        let (backend, mode) = crate::select(&cfg.output, caps);
        Self {
            backend,
            popups: popups.filter(|_| backend == Backend::Popup),
            notifier: notifier
                .filter(|_| matches!(mode, Some(OutputMode::Notification | OutputMode::Both))),
        }
    }
}

#[instrument(skip_all)]
async fn open_notifier(cfg: &UiConfig) -> Option<Notifier> {
    match Notifier::connect(&cfg.notification).await {
        Ok(notifier) => Some(notifier),
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "notifications unavailable"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use touchcue_core::EndReason;
    use touchcue_core::config::{OutputTarget, Position};

    use super::*;
    use crate::Ui;

    const CHILD_ENV: &str = "TOUCHCUE_UI_PROBE_CHILD";

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Ui(#[from] UiError),
        #[error("probe child timed out")]
        ChildTimeout,
        #[error("failed to create a test fixture")]
        Fixture,
        #[error("failed to encode the PNG fixture")]
        Encode(#[from] png::EncodingError),
        #[error(transparent)]
        Output(#[from] OutputError),
        #[error("no display server accepted popups")]
        NoDisplay,
    }

    /// Runs [`probe`] in a child process without a Wayland or session bus
    /// environment, so this process's environment is never mutated.
    #[test]
    fn probe_without_desktop_does_not_panic() -> Result<(), TestError> {
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "linux::tests::probe_child", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(TestError::ChildTimeout);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "probe child failed: {status}");
        Ok(())
    }

    #[tokio::test]
    async fn probe_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let caps = probe().await;
        // The session bus falls back to /run/user/<uid>/bus, so only Wayland is predictable.
        assert!(!caps.wayland && !caps.layer_shell);
    }

    fn prompt(id: u64, title: &str, body: &str, state: RequestState) -> Prompt {
        Prompt {
            id: RequestId(id),
            title: title.to_owned(),
            body: body.to_owned(),
            icon: None,
            state,
        }
    }

    /// Shows two stacked popups for about 3 s on the running compositor.
    #[tokio::test]
    #[ignore = "needs a Wayland compositor with layer-shell"]
    async fn popup_smoke() -> Result<(), TestError> {
        let mut cfg = UiConfig::default();
        cfg.output.fallback = OutputMode::Off;
        let icon =
            std::env::temp_dir().join(format!("touchcue-ui-smoke-{}.png", std::process::id()));
        let mut pixmap = tiny_skia::Pixmap::new(32, 32).ok_or(TestError::Fixture)?;
        pixmap.fill(tiny_skia::Color::from_rgba8(40, 160, 90, 255));
        std::fs::write(&icon, pixmap.encode_png()?)?;
        let ui = Ui::spawn(cfg, CancellationToken::new()).await?;
        assert_eq!(ui.backend(), Backend::Popup);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut first = prompt(
            1,
            "Touch your security key",
            "touchcue popup_smoke is waiting for a FIDO2 touch; this text wraps onto a second line",
            RequestState::Waiting,
        );
        first.icon = Some(icon.clone());
        ui.send(Command::Show(first)).await?;
        let second = |state| prompt(2, "Second prompt", "stacked below the first", state);
        ui.send(Command::Show(second(RequestState::Waiting)))
            .await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let cancelled = RequestState::Lingering(EndReason::Cancelled);
        ui.send(Command::Update(second(cancelled))).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        ui.send(Command::Hide(RequestId(1))).await?;
        ui.send(Command::Hide(RequestId(2))).await?;
        ui.shutdown(Instant::now() + SHUTDOWN_TIMEOUT).await?;
        std::fs::remove_file(&icon)?;
        Ok(())
    }

    /// Shows two prompts with `popup` for about 3 s on the running desktop,
    /// ending the second one halfway.
    async fn placement_smoke(popup: touchcue_core::config::Popup) -> Result<(), TestError> {
        let mut cfg = UiConfig::default();
        cfg.output.fallback = OutputMode::Off;
        cfg.popup = popup;
        let ui = Ui::spawn(cfg, CancellationToken::new()).await?;
        assert_eq!(ui.backend(), Backend::Popup);
        ui.send(Command::Show(prompt(
            1,
            "Touch your security key",
            "touchcue placement smoke test; this text wraps onto a second line",
            RequestState::Waiting,
        )))
        .await?;
        let second = |state| prompt(2, "Second prompt", "stacked below the first", state);
        ui.send(Command::Show(second(RequestState::Waiting)))
            .await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let touched = RequestState::Lingering(EndReason::Touched);
        ui.send(Command::Update(second(touched))).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        ui.send(Command::Hide(RequestId(1))).await?;
        ui.send(Command::Hide(RequestId(2))).await?;
        ui.shutdown(Instant::now() + SHUTDOWN_TIMEOUT).await?;
        Ok(())
    }

    /// Shows two popups stacked at the centre of the focused output.
    #[tokio::test]
    #[ignore = "draws on the running desktop"]
    async fn center_focused_smoke() -> Result<(), TestError> {
        placement_smoke(touchcue_core::config::Popup::default()).await
    }

    /// Shows two popups stacked at the centre of every output.
    #[tokio::test]
    #[ignore = "draws on the running desktop"]
    async fn center_all_smoke() -> Result<(), TestError> {
        placement_smoke(touchcue_core::config::Popup {
            output: OutputTarget::All,
            ..touchcue_core::config::Popup::default()
        })
        .await
    }

    /// Shows two popups in the top-right corner of the output under the
    /// mouse pointer; outside Hyprland this falls back to the focused output.
    #[tokio::test]
    #[ignore = "draws on the running desktop"]
    async fn cursor_smoke() -> Result<(), TestError> {
        placement_smoke(touchcue_core::config::Popup {
            position: Position::TopRight,
            output: OutputTarget::Cursor,
            ..touchcue_core::config::Popup::default()
        })
        .await
    }

    /// Handles display events and sends requests for `duration`.
    async fn pump(output: &mut PopupOutput, duration: Duration) -> Result<(), OutputError> {
        let end = Instant::now() + duration;
        loop {
            output.dispatch().await?;
            match tokio::time::timeout_at(end, output.read()).await {
                Ok(incoming) => output.handle(incoming?)?,
                Err(_deadline_reached) => return Ok(()),
            }
        }
    }

    /// Shows two centred modal popups on the focused output, which is dimmed
    /// and takes no clicks for about 3 s; a click on the dim area hides both.
    /// Fails when the overlay is not created and drawn.
    #[tokio::test]
    #[ignore = "draws on the running desktop and blocks its clicks for about 3 s"]
    async fn modal_smoke() -> Result<(), TestError> {
        let popup = touchcue_core::config::Popup {
            modal: true,
            ..touchcue_core::config::Popup::default()
        };
        let mut output = PopupOutput::open(&Placement::new(&popup))
            .await
            .ok_or(TestError::NoDisplay)?;
        let second = |state| prompt(2, "Second prompt", "stacked below the first", state);
        output
            .show(
                &prompt(
                    1,
                    "Touch your security key",
                    "touchcue modal_smoke: the output is dimmed and takes no clicks",
                    RequestState::Waiting,
                ),
                true,
            )
            .await?;
        output.show(&second(RequestState::Waiting), true).await?;
        pump(&mut output, Duration::from_millis(1500)).await?;
        let (overlays, drawn) = output.overlay_counts();
        println!(
            "modal overlays: {overlays}, drawn: {drawn}, as {}",
            output.overlay_paint()
        );
        assert_eq!((overlays, drawn), (1, 1), "overlay missing or not drawn");
        let touched = RequestState::Lingering(EndReason::Touched);
        output.update(&second(touched), false).await?;
        pump(&mut output, Duration::from_millis(1500)).await?;
        let (overlays, drawn) = output.overlay_counts();
        println!("after 3 s: modal overlays: {overlays}, drawn: {drawn}");
        output.hide(RequestId(1))?;
        output.hide(RequestId(2))?;
        assert_eq!(
            output.overlay_counts(),
            (0, 0),
            "overlay outlived its requests"
        );
        output.dispatch().await?;
        Ok(())
    }

    /// Shows a popup and a transient notification for about 3 s on the
    /// running desktop.
    #[tokio::test]
    #[ignore = "needs a Wayland compositor with layer-shell and a notification server"]
    async fn notification_smoke() -> Result<(), TestError> {
        let mut cfg = UiConfig::default();
        cfg.output.mode = OutputMode::Both;
        cfg.output.fallback = OutputMode::Off;
        cfg.notification.safety_timeout_s = 10;
        let ui = Ui::spawn(cfg, CancellationToken::new()).await?;
        assert_eq!(ui.backend(), Backend::Popup);
        let smoke = |state| {
            prompt(
                7,
                "touchcue notification_smoke",
                "Waiting for a <test> touch & more",
                state,
            )
        };
        ui.send(Command::Show(smoke(RequestState::Waiting))).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let timed_out = RequestState::Lingering(EndReason::TimedOut);
        ui.send(Command::Update(smoke(timed_out))).await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        ui.send(Command::Hide(RequestId(7))).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        ui.shutdown(Instant::now() + SHUTDOWN_TIMEOUT).await?;
        Ok(())
    }
}
