//! Popup, notification and command output backends.
//!
//! On Linux one tokio task owns the Wayland or X11 connection and every
//! shown popup, and a second task owns the session bus connection. Other targets
//! compile a stub whose backend is always [`Backend::Off`].

use std::path::PathBuf;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use touchcue_core::config::{Notification, Output, OutputMode, Popup};
use touchcue_core::{RequestId, RequestState};
use tracing::{debug, info, instrument, warn};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod stub;
#[cfg(target_os = "linux")]
mod text;
#[cfg(target_os = "linux")]
mod timing;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(not(target_os = "linux"))]
use stub as platform;

/// Time the UI task is given to stop after its commands channel closes or
/// its cancellation token fires.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest time [`Ui::spawn`] spends finding and opening outputs.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest time [`Ui::send`] waits to queue a [`Command::Hide`].
pub const HIDE_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// Commands queued for the UI task.
pub const QUEUE_LEN: usize = 64;

/// Text and state of one touch prompt; the text is shown unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub id: RequestId,
    pub title: String,
    pub body: String,
    /// PNG or SVG file shown next to the text, or used as the notification icon.
    pub icon: Option<PathBuf>,
    pub state: RequestState,
}

/// Instruction sent to the UI task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Shows a prompt after the configured delay; a shown id is updated instead.
    Show(Prompt),
    /// Replaces the text of a pending or shown prompt in place; an unknown id is ignored.
    Update(Prompt),
    /// Withdraws a prompt once it has been visible for the minimum display time.
    Hide(RequestId),
}

/// Primary output chosen by [`Ui::spawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Overlay popups through Wayland layer-shell, else X11 override-redirect
    /// windows; with output mode `both`, notifications are shown as well.
    Popup,
    /// Freedesktop notifications over the session bus.
    Notification,
    /// Nothing is shown and commands are discarded.
    Off,
}

/// Output configuration of the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiConfig {
    pub output: Output,
    pub popup: Popup,
    pub notification: Notification,
}

/// Desktop features found by [`probe`].
#[expect(
    clippy::struct_excessive_bools,
    reason = "each field is an independent probe result"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// A Wayland compositor accepted a connection.
    pub wayland: bool,
    /// The compositor offers `zwlr_layer_shell_v1`.
    pub layer_shell: bool,
    /// An X11 display accepted a connection and supports popup windows.
    pub x11: bool,
    /// `org.freedesktop.Notifications` has an owner on the session bus or
    /// the bus can activate it.
    pub notifications: bool,
}

impl Capabilities {
    /// Reports whether some display server can show popups.
    #[must_use]
    pub fn popups(&self) -> bool {
        self.layer_shell || self.x11
    }
}

/// A UI task failure.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("finding and opening outputs took longer than {timeout:?}")]
    InitTimeout {
        timeout: Duration,
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("the UI task is not running")]
    Disconnected,
    #[error("the UI command queue is full")]
    QueueFull,
    #[error("queueing a hide took longer than {timeout:?}")]
    SendTimeout {
        timeout: Duration,
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("the UI task did not stop by the deadline and was aborted")]
    ShutdownTimeout(#[source] tokio::time::error::Elapsed),
    #[error("the UI task failed")]
    Task(#[source] tokio::task::JoinError),
}

/// Handle to the UI task; dropping it stops the task without waiting.
#[derive(Debug)]
pub struct Ui {
    backend: Backend,
    inner: Option<platform::Handle>,
}

impl Ui {
    /// Finds the first available output and starts the UI task.
    ///
    /// The backend is [`choose`] applied to the outputs that opened within
    /// [`INIT_TIMEOUT`]. With output mode `both`, notifications are shown
    /// next to popups when the notification service is available. With
    /// [`Backend::Off`] no task is started. The task stops, withdrawing
    /// every prompt, when `cancel` fires or the [`Ui`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`UiError::InitTimeout`] when the outputs do not open in time.
    #[instrument(
        skip_all,
        fields(mode = ?cfg.output.mode, fallback = ?cfg.output.fallback),
        err
    )]
    pub async fn spawn(cfg: UiConfig, cancel: CancellationToken) -> Result<Self, UiError> {
        let (backend, inner) = platform::spawn(cfg, cancel).await?;
        Ok(Self { backend, inner })
    }

    /// Returns the backend chosen by [`Ui::spawn`].
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Queues a command.
    ///
    /// [`Command::Show`] and [`Command::Update`] never wait; [`Command::Hide`]
    /// waits up to [`HIDE_SEND_TIMEOUT`] for queue space so it is not lost.
    /// Commands are discarded when the backend is [`Backend::Off`].
    ///
    /// # Errors
    ///
    /// Returns [`UiError::QueueFull`] when a show or update finds the queue
    /// full, [`UiError::SendTimeout`] when a hide cannot be queued in time,
    /// and [`UiError::Disconnected`] when the UI task has exited.
    pub async fn send(&self, cmd: Command) -> Result<(), UiError> {
        match &self.inner {
            Some(handle) => handle.send(cmd).await,
            None => Ok(()),
        }
    }

    /// Withdraws every prompt and stops the UI task, aborting it if it has
    /// not finished by `deadline`.
    ///
    /// # Errors
    ///
    /// Returns [`UiError::ShutdownTimeout`] when the task was aborted and
    /// [`UiError::Task`] when it panicked.
    pub async fn shutdown(self, deadline: Instant) -> Result<(), UiError> {
        match self.inner {
            Some(handle) => handle.shutdown(deadline).await,
            None => Ok(()),
        }
    }
}

/// Returns the backend [`Ui::spawn`] uses for `output` on a desktop with `caps`.
///
/// `output.mode` is tried first, then `output.fallback`. Modes `popup` and
/// `both` need layer-shell, mode `notification` needs the notification
/// service, mode `command` is unsupported and skipped with a warning, and
/// mode `none` selects [`Backend::Off`]. When neither mode is available the
/// backend is [`Backend::Off`].
#[must_use]
pub fn choose(output: &Output, caps: &Capabilities) -> Backend {
    select(output, *caps).0
}

/// Returns the backend of [`choose`] with the mode that selected it.
#[instrument(skip_all, fields(mode = ?output.mode, fallback = ?output.fallback, ?caps))]
fn select(output: &Output, caps: Capabilities) -> (Backend, Option<OutputMode>) {
    for mode in [output.mode, output.fallback] {
        let backend = match mode {
            OutputMode::Popup | OutputMode::Both if caps.popups() => Backend::Popup,
            OutputMode::Notification if caps.notifications => Backend::Notification,
            OutputMode::Off => Backend::Off,
            OutputMode::Command => {
                warn!("output mode `command` is not supported; skipping it");
                continue;
            }
            OutputMode::Popup | OutputMode::Both | OutputMode::Notification => {
                debug!(?mode, "output mode unavailable");
                continue;
            }
        };
        info!(?mode, ?backend, "chose output backend");
        return (backend, Some(mode));
    }
    info!(backend = ?Backend::Off, "no output mode available");
    (Backend::Off, None)
}

/// Reports which outputs the desktop supports without showing anything.
///
/// Each check gives up after [`INIT_TIMEOUT`].
pub async fn probe() -> Capabilities {
    platform::probe().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(mode: OutputMode, fallback: OutputMode) -> Output {
        Output { mode, fallback }
    }

    const ALL: Capabilities = Capabilities {
        wayland: true,
        layer_shell: true,
        x11: true,
        notifications: true,
    };
    const NOTIFY_ONLY: Capabilities = Capabilities {
        wayland: false,
        layer_shell: false,
        x11: false,
        notifications: true,
    };
    const NONE: Capabilities = Capabilities {
        wayland: false,
        layer_shell: false,
        x11: false,
        notifications: false,
    };
    const X11_ONLY: Capabilities = Capabilities {
        wayland: true,
        layer_shell: false,
        x11: true,
        notifications: false,
    };

    #[test]
    fn mode_wins_when_available() {
        let popup = output(OutputMode::Popup, OutputMode::Notification);
        assert_eq!(choose(&popup, &ALL), Backend::Popup);
        let both = output(OutputMode::Both, OutputMode::Off);
        assert_eq!(choose(&both, &ALL), Backend::Popup);
        let notify = output(OutputMode::Notification, OutputMode::Popup);
        assert_eq!(choose(&notify, &ALL), Backend::Notification);
    }

    #[test]
    fn fallback_applies_when_mode_is_unavailable() {
        let popup = output(OutputMode::Popup, OutputMode::Notification);
        assert_eq!(choose(&popup, &NOTIFY_ONLY), Backend::Notification);
        assert_eq!(choose(&popup, &NONE), Backend::Off);
        let both = output(OutputMode::Both, OutputMode::Notification);
        assert_eq!(choose(&both, &NOTIFY_ONLY), Backend::Notification);
    }

    #[test]
    fn x11_provides_popups_without_layer_shell() {
        let popup = output(OutputMode::Popup, OutputMode::Notification);
        assert_eq!(choose(&popup, &X11_ONLY), Backend::Popup);
    }

    #[test]
    fn command_and_none_show_nothing() {
        let command = output(OutputMode::Command, OutputMode::Notification);
        assert_eq!(choose(&command, &ALL), Backend::Notification);
        let off = output(OutputMode::Off, OutputMode::Popup);
        assert_eq!(choose(&off, &ALL), Backend::Off);
        let both_command = output(OutputMode::Command, OutputMode::Command);
        assert_eq!(choose(&both_command, &ALL), Backend::Off);
    }
}
