//! Freedesktop notifications over the session bus.
//!
//! Specification: <https://specifications.freedesktop.org/notification/latest/>

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use touchcue_core::config::{Notification, Urgency};
use touchcue_core::{RateLimit, RequestId};
use tracing::{Instrument, debug, info_span, instrument, warn};
use zbus::zvariant::Value;

use super::WARN_INTERVAL;
use crate::Prompt;
use crate::text::{display_body, escape_markup};

const APP_NAME: &str = "touchcue";
const DEFAULT_ICON: &str = "security-high";
pub(crate) const SERVICE: &str = "org.freedesktop.Notifications";
/// Longest time one bus call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest time the notifier task spends on one command.
const COMMAND_BUDGET: Duration = Duration::from_secs(2);
/// Commands queued for the notifier task before new ones are dropped.
const QUEUE_LEN: usize = 64;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[expect(clippy::too_many_arguments, reason = "mirrors the D-Bus method")]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;
}

/// Why notifications are unavailable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotifyError {
    #[error("session bus call failed")]
    Bus(#[from] zbus::Error),
    #[error("session bus refused a query")]
    Fdo(#[from] zbus::fdo::Error),
    #[error("invalid bus name")]
    Name(#[from] zbus::names::Error),
    #[error("no notification server is running or activatable")]
    NoService,
}

/// Opens a session bus connection whose calls time out after [`CALL_TIMEOUT`].
pub(crate) async fn session() -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::session()?
        .method_timeout(CALL_TIMEOUT)
        .build()
        .await
}

/// Reports whether a notification server owns [`SERVICE`] or the bus can activate one.
#[instrument(skip_all)]
pub(crate) async fn service_available(conn: &zbus::Connection) -> Result<bool, NotifyError> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    if dbus.name_has_owner(SERVICE.try_into()?).await? {
        return Ok(true);
    }
    let activatable = dbus.list_activatable_names().await?;
    Ok(activatable.iter().any(|name| name.as_str() == SERVICE))
}

/// One notification per shown request, replaced in place on update.
pub(crate) struct Notifier {
    proxy: NotificationsProxy<'static>,
    urgency: u8,
    expire_timeout: i32,
    /// Whether the server parses body markup; unknown until a query succeeds.
    markup: Option<bool>,
    ids: BTreeMap<RequestId, u32>,
    /// Limits warnings about failing calls.
    warn_limit: RateLimit,
}

impl Notifier {
    /// Connects to the session bus.
    ///
    /// Returns [`NotifyError::NoService`] when no notification server runs
    /// and none can be activated.
    #[instrument(skip_all, fields(urgency = ?cfg.urgency))]
    pub(crate) async fn connect(cfg: &Notification) -> Result<Self, NotifyError> {
        let conn = session().await?;
        if !service_available(&conn).await? {
            return Err(NotifyError::NoService);
        }
        let expire_ms = cfg.safety_timeout_s.saturating_mul(1000);
        let expire_timeout = match i32::try_from(expire_ms) {
            Ok(timeout) => timeout,
            Err(err) => {
                warn!(
                    error = &err as &dyn std::error::Error,
                    expire_ms, "notification timeout out of range; clamping it"
                );
                i32::MAX
            }
        };
        Ok(Self {
            proxy: NotificationsProxy::new(&conn).await?,
            urgency: match cfg.urgency {
                Urgency::Low => 0,
                Urgency::Normal => 1,
                Urgency::Critical => 2,
            },
            expire_timeout,
            markup: None,
            ids: BTreeMap::new(),
            warn_limit: RateLimit::new(WARN_INTERVAL),
        })
    }

    /// Shows the prompt, replacing the notification already shown for its id.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    async fn show(&mut self, prompt: &Prompt) {
        let markup = if let Some(markup) = self.markup {
            markup
        } else {
            match self.call(&Call::Capabilities).await {
                Some(Reply::Capabilities(caps)) => {
                    let markup = caps.iter().any(|cap| cap == "body-markup");
                    debug!(markup, "notification server capabilities");
                    self.markup = Some(markup);
                    markup
                }
                // Unknown capabilities are queried again next time; escaping is always safe.
                Some(Reply::Notified(_) | Reply::Closed) | None => true,
            }
        };
        let body = display_body(prompt);
        let body = if markup { escape_markup(&body) } else { body };
        let icon = prompt
            .icon
            .as_deref()
            .and_then(|path| path.to_str())
            .unwrap_or(DEFAULT_ICON);
        let replaces_id = self.ids.get(&prompt.id).copied().unwrap_or(0);
        let call = Call::Notify {
            replaces_id,
            icon,
            title: &prompt.title,
            body: &body,
            urgency: self.urgency,
            expire_timeout: self.expire_timeout,
        };
        if let Some(Reply::Notified(notification)) = self.call(&call).await {
            debug!(
                id = %prompt.id,
                notification,
                replaces = replaces_id,
                "notification shown"
            );
            self.ids.insert(prompt.id, notification);
        }
    }

    /// Closes the notification shown for `id` and forgets it.
    #[instrument(level = "debug", skip_all, fields(%id))]
    async fn hide(&mut self, id: RequestId) {
        if let Some(notification) = self.ids.remove(&id) {
            let closed = self.call(&Call::Close(notification)).await;
            debug!(%id, notification, closed = closed.is_some(), "notification closed");
        }
    }

    /// Closes every shown notification.
    async fn hide_all(&mut self) {
        let ids: Vec<_> = self.ids.keys().copied().collect();
        for id in ids {
            self.hide(id).await;
        }
    }

    /// Runs a call, reconnecting and retrying it once when the bus
    /// connection failed rather than the server answering with an error.
    ///
    /// Returns `None` after logging the failure.
    #[instrument(level = "debug", skip_all, fields(method = call.method()))]
    async fn call(&mut self, call: &Call<'_>) -> Option<Reply> {
        let method = call.method();
        let err = match call.invoke(&self.proxy).await {
            Ok(reply) => return Some(reply),
            Err(err) => err,
        };
        if is_reply(&err) {
            self.warn_limit
                .log(std::time::Instant::now(), |suppressed| {
                    warn!(
                        method,
                        error = &err as &dyn std::error::Error,
                        suppressed,
                        "notification server rejected a call"
                    );
                });
            return None;
        }
        debug!(
            method,
            error = &err as &dyn std::error::Error,
            "notification call failed; reconnecting"
        );
        let retried = match proxy().await {
            Ok(proxy) => {
                self.proxy = proxy;
                self.markup = None;
                call.invoke(&self.proxy).await
            }
            Err(err) => Err(err),
        };
        match retried {
            Ok(reply) => Some(reply),
            Err(err) => {
                self.warn_limit
                    .log(std::time::Instant::now(), |suppressed| {
                        warn!(
                            method,
                            error = &err as &dyn std::error::Error,
                            suppressed,
                            "notification call failed after reconnecting"
                        );
                    });
                None
            }
        }
    }
}

/// One method call on the notification server.
enum Call<'a> {
    Capabilities,
    Notify {
        replaces_id: u32,
        icon: &'a str,
        title: &'a str,
        body: &'a str,
        urgency: u8,
        expire_timeout: i32,
    },
    Close(u32),
}

enum Reply {
    Capabilities(Vec<String>),
    Notified(u32),
    Closed,
}

impl Call<'_> {
    fn method(&self) -> &'static str {
        match self {
            Self::Capabilities => "GetCapabilities",
            Self::Notify { .. } => "Notify",
            Self::Close(_) => "CloseNotification",
        }
    }

    async fn invoke(&self, proxy: &NotificationsProxy<'static>) -> zbus::Result<Reply> {
        match *self {
            Self::Capabilities => proxy.get_capabilities().await.map(Reply::Capabilities),
            Self::Notify {
                replaces_id,
                icon,
                title,
                body,
                urgency,
                expire_timeout,
            } => {
                let hints = HashMap::from([
                    ("urgency", Value::U8(urgency)),
                    ("transient", Value::Bool(true)),
                    ("desktop-entry", Value::from(APP_NAME)),
                ]);
                proxy
                    .notify(
                        APP_NAME,
                        replaces_id,
                        icon,
                        title,
                        body,
                        &[],
                        hints,
                        expire_timeout,
                    )
                    .await
                    .map(Reply::Notified)
            }
            Self::Close(notification) => proxy
                .close_notification(notification)
                .await
                .map(|()| Reply::Closed),
        }
    }
}

/// Reports whether the server received the call and answered with an error.
fn is_reply(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::MethodError(..) | zbus::Error::FDO(_))
}

async fn proxy() -> zbus::Result<NotificationsProxy<'static>> {
    NotificationsProxy::new(&session().await?).await
}

enum Job {
    Show(Prompt),
    Hide(RequestId),
}

/// Runs a [`Notifier`] on its own task so a slow server never delays popups.
pub(crate) struct NotifyWorker {
    jobs: mpsc::Sender<Job>,
    stop: CancellationToken,
    task: JoinHandle<()>,
    warn_limit: RateLimit,
}

impl NotifyWorker {
    /// Spawns the notifier task on the current runtime.
    pub(crate) fn start(notifier: Notifier) -> Self {
        let (jobs, queue) = mpsc::channel(QUEUE_LEN);
        let stop = CancellationToken::new();
        let task =
            tokio::spawn(run(notifier, queue, stop.clone()).instrument(info_span!("notifier")));
        Self {
            jobs,
            stop,
            task,
            warn_limit: RateLimit::new(WARN_INTERVAL),
        }
    }

    /// Queues showing or updating a prompt.
    pub(crate) fn show(&mut self, prompt: &Prompt) {
        self.queue(Job::Show(prompt.clone()), prompt.id);
    }

    /// Queues closing the notification of `id`.
    pub(crate) fn hide(&mut self, id: RequestId) {
        self.queue(Job::Hide(id), id);
    }

    fn queue(&mut self, job: Job, id: RequestId) {
        if let Err(err) = self.jobs.try_send(job) {
            let reason = match err {
                TrySendError::Full(_) => "queue full",
                TrySendError::Closed(_) => "task stopped",
            };
            self.warn_limit
                .log(std::time::Instant::now(), |suppressed| {
                    warn!(%id, reason, suppressed, "notification command dropped");
                });
        }
    }

    /// Skips queued commands, closes shown notifications and waits for the
    /// task until `deadline`, aborting it then.
    #[instrument(skip_all)]
    pub(crate) async fn shutdown(mut self, deadline: Instant) {
        self.stop.cancel();
        match tokio::time::timeout_at(deadline, &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(
                error = &err as &dyn std::error::Error,
                "notifier task failed"
            ),
            Err(_) => {
                self.task.abort();
                warn!("notifier task did not stop in time; aborted it");
            }
        }
    }
}

async fn run(mut notifier: Notifier, mut queue: mpsc::Receiver<Job>, stop: CancellationToken) {
    let mut warn_limit = RateLimit::new(WARN_INTERVAL);
    loop {
        let job = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            job = queue.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        let (id, done) = match job {
            Job::Show(prompt) => {
                let id = prompt.id;
                (
                    id,
                    tokio::time::timeout(COMMAND_BUDGET, notifier.show(&prompt)).await,
                )
            }
            Job::Hide(id) => (
                id,
                tokio::time::timeout(COMMAND_BUDGET, notifier.hide(id)).await,
            ),
        };
        if let Err(elapsed) = &done {
            warn_limit.log(std::time::Instant::now(), |suppressed| {
                warn!(
                    %id,
                    budget_ms = COMMAND_BUDGET.as_millis(),
                    error = elapsed as &dyn std::error::Error,
                    suppressed,
                    "notification command took too long; abandoned it"
                );
            });
        }
    }
    notifier.hide_all().await;
}
