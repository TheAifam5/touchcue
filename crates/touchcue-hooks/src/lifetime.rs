//! Hooks with `until = "ended"`: a process per request that lives while the
//! request's prompt is shown.
//!
//! A hook's process for a request starts once one of the hook's `on` events
//! fired for the request with matching values and the prompt is shown. It
//! is stopped when a rule suppresses the prompt, and started again when the
//! prompt is shown again; it is stopped for good when the request ends. The
//! configured argv runs unchanged, without a shell. A process gets the
//! prompt in its environment, as a hook does plus [`TITLE_VAR`] and
//! [`BODY_VAR`], and changes on stdin as JSON Lines:
//! `{"op":"show"|"update","id":…,"title":…,"body":…,"values":{…}}` and
//! `{"op":"hide","id":…}`. Writing never blocks the daemon: each process
//! has a queue of [`STDIN_QUEUE`] lines that drops the oldest update when
//! full, never a show or a hide.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, watch};
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use touchcue_core::text::{outcome_body, sanitize};
use touchcue_core::{Hook, HookEvent, Lifetime, OnChange, RequestId, RequestState};
use tracing::Instrument as _;

use crate::platform::{self, Stdin};
use crate::queue::{Droppable, Pushed, Queue};
use crate::runner::{Ending, Stop, Warnings, millis};
use crate::{EVENT_VAR, VALUE_MAX, published_values, vars};

/// Name of the variable holding the rendered prompt title.
pub const TITLE_VAR: &str = "TOUCHCUE_TITLE";
/// Name of the variable holding the rendered prompt body.
pub const BODY_VAR: &str = "TOUCHCUE_BODY";
pub use touchcue_core::config::LIFETIME_PROCESSES;
/// Lines queued for the stdin of one process.
pub const STDIN_QUEUE: usize = 64;
/// Changes queued for one hook.
const MAILBOX: usize = 256;
/// Longest time spent writing the lines left for a process whose prompt
/// went away before it is stopped.
const CLOSE_FLUSH: Duration = Duration::from_millis(200);
/// The placeholder whose change alone does not restart a process.
const ELAPSED_KEY: &str = "request.elapsed";

/// Kind of the line or environment that starts a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Show,
    Update,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Update => "update",
        }
    }
}

/// A request's rendered prompt and published values, sanitized.
#[derive(Debug)]
struct Payload {
    id: RequestId,
    /// The prompt is shown; false while a rule suppresses it.
    shown: bool,
    title: Option<String>,
    body: Option<String>,
    values: BTreeMap<String, String>,
}

/// The rendered prompt of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptText<'a> {
    pub title: &'a str,
    /// The body without the outcome, which [`outcome_body`] adds for `state`.
    pub body: &'a str,
    pub state: RequestState,
}

impl Payload {
    fn new(
        id: RequestId,
        prompt: Option<PromptText<'_>>,
        values: &BTreeMap<String, String>,
    ) -> Self {
        Self {
            id,
            shown: prompt.is_some(),
            title: prompt.and_then(|prompt| sanitize(prompt.title, VALUE_MAX)),
            body: prompt
                .and_then(|prompt| sanitize(&outcome_body(prompt.body, prompt.state), VALUE_MAX)),
            values: published_values(values),
        }
    }

    /// Returns whether a value other than the elapsed time differs from `other`.
    fn changed(&self, other: &Self) -> bool {
        let key = |payload: &Self| {
            payload
                .values
                .iter()
                .filter(|(key, _)| key.as_str() != ELAPSED_KEY)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>()
        };
        key(self) != key(other)
    }

    /// Returns the environment of a process started for `op`.
    fn env(&self, op: Op) -> Vec<(String, String)> {
        let mut env = vec![(EVENT_VAR.to_owned(), op.as_str().to_owned())];
        for (var, value) in [(TITLE_VAR, &self.title), (BODY_VAR, &self.body)] {
            if let Some(value) = value {
                env.push((var.to_owned(), value.clone()));
            }
        }
        env.extend(vars(&self.values));
        env
    }

    /// Returns the stdin line of `op`.
    fn line(&self, op: Op) -> Line {
        let json = serde_json::json!({
            "op": op.as_str(),
            "id": self.id.0,
            "title": self.title.as_deref().unwrap_or_default(),
            "body": self.body.as_deref().unwrap_or_default(),
            "values": self.values,
        });
        Line {
            text: format!("{json}\n"),
            droppable: op == Op::Update,
        }
    }
}

/// The stdin line of hiding request `id`.
fn hide_line(id: RequestId) -> Line {
    let json = serde_json::json!({ "op": "hide", "id": id.0 });
    Line {
        text: format!("{json}\n"),
        droppable: false,
    }
}

/// One JSON line for the stdin of a process.
#[derive(Debug)]
struct Line {
    text: String,
    droppable: bool,
}

impl Droppable for Line {
    fn droppable(&self) -> bool {
        self.droppable
    }
}

/// A change queued for a hook.
#[derive(Debug)]
enum Msg {
    /// The request changed; `arm` when one of the hook's `on` events fired
    /// with matching values.
    Change { payload: Arc<Payload>, arm: bool },
    /// The request ended.
    End(RequestId),
}

impl Droppable for Msg {
    fn droppable(&self) -> bool {
        matches!(self, Self::Change { arm: false, .. })
    }
}

/// One hook with `until` and its queue of changes.
#[derive(Debug)]
struct Entry {
    index: usize,
    hook: Hook,
    lifetime: Lifetime,
    mailbox: Queue<Msg>,
    /// Limits the full-queue warnings of this hook across every sender.
    full: Warnings,
    /// Limits the warnings of processes of this hook that could not start.
    failures: Arc<Warnings>,
}

/// Queues request changes for the hooks with `until` without waiting.
#[derive(Debug, Clone)]
pub(crate) struct Lifetimes {
    entries: Arc<[Arc<Entry>]>,
}

impl Lifetimes {
    /// Queues a change of request `id`; see [`crate::HookSender::request`].
    ///
    /// When a hook's queue is full, its oldest queued change that does not
    /// start the process is dropped, with a warning rate-limited per hook.
    pub(crate) fn request(
        &self,
        id: RequestId,
        events: &[HookEvent],
        values: &BTreeMap<String, String>,
        prompt: Option<PromptText<'_>>,
    ) {
        if self.entries.is_empty() {
            return;
        }
        let payload = (!events.contains(&HookEvent::Ended))
            .then(|| Arc::new(Payload::new(id, prompt, values)));
        for entry in self.entries.iter() {
            let msg = match &payload {
                Some(payload) => Msg::Change {
                    payload: Arc::clone(payload),
                    arm: events
                        .iter()
                        .any(|event| entry.hook.applies(*event, values)),
                },
                None => Msg::End(id),
            };
            match entry.mailbox.push(msg) {
                Pushed::Queued => {}
                Pushed::Dropped | Pushed::Stalled => {
                    if let Some(suppressed) = entry.full.check() {
                        tracing::warn!(
                            hook = entry.index,
                            limit = MAILBOX,
                            suppressed,
                            "hook queue full; dropping a request change"
                        );
                    }
                }
                Pushed::Closed => {
                    tracing::debug!(hook = entry.index, "hooks stopped; dropping a change");
                }
            }
        }
    }
}

/// Starts a task per hook of `hooks`, each with its configuration index,
/// in `tracker`; the tasks stop their processes once `stop` is cancelled.
/// At most `limit` processes of all of them run at once.
pub(crate) fn spawn(
    hooks: Vec<(usize, Hook)>,
    tracker: &TaskTracker,
    stop: &CancellationToken,
    limit: usize,
) -> Lifetimes {
    let permits = Arc::new(Semaphore::new(limit));
    let freed = Arc::new(watch::Sender::new(0_u64));
    let entries = hooks
        .into_iter()
        .filter_map(|(index, hook)| {
            let lifetime = hook.lifetime?;
            let entry = Arc::new(Entry {
                index,
                hook,
                lifetime,
                mailbox: Queue::new(MAILBOX),
                full: Warnings::new(),
                failures: Arc::new(Warnings::new()),
            });
            let on_change = lifetime.on_change.as_str();
            let worker = Worker {
                entry: Arc::clone(&entry),
                stop: stop.child_token(),
                permits: Arc::clone(&permits),
                freed_rx: freed.subscribe(),
                freed: Arc::clone(&freed),
                blocked: false,
                tasks: JoinSet::new(),
                busy: Warnings::new(),
                dropped: Warnings::new(),
                stalled: Warnings::new(),
            };
            tracker.spawn(worker.run().instrument(tracing::info_span!(
                "hooks",
                hook = index,
                on_change
            )));
            Some(entry)
        })
        .collect();
    Lifetimes { entries }
}

/// A started process.
#[derive(Debug)]
struct Proc {
    /// Task that runs the process.
    task: task::Id,
    lines: Arc<Queue<Line>>,
    /// Stops the process at once.
    stop: CancellationToken,
    /// Stops the process once its queued lines are written, or after
    /// [`CLOSE_FLUSH`].
    closing: CancellationToken,
    /// The process was told to stop.
    stopping: bool,
    /// The process was stopped for not reading stdin.
    stalled: bool,
}

impl Proc {
    /// Ends the process after writing its queued lines and `last`.
    fn close(&mut self, last: Line) {
        self.lines.push(last);
        self.lines.close();
        self.closing.cancel();
        self.stopping = true;
    }

    /// Ends the process at once.
    fn kill(&mut self) {
        self.stop.cancel();
        self.stopping = true;
    }
}

/// State of one request for one hook.
#[derive(Debug)]
struct Slot {
    payload: Arc<Payload>,
    proc: Option<Proc>,
    /// Values the process has been given: those it was started with, and
    /// with `stream` each update queued for its stdin.
    started: Option<Arc<Payload>>,
    last_start: Option<Instant>,
    /// The process exited on its own or stopped reading stdin; it is not
    /// started again until the hook is armed again or, unless `on_change`
    /// is `ignore`, a value changes.
    dismissed: bool,
}

/// Starts and stops the processes of one hook.
struct Worker {
    entry: Arc<Entry>,
    stop: CancellationToken,
    permits: Arc<Semaphore>,
    /// Changes whenever a process of any hook released its permit.
    freed_rx: watch::Receiver<u64>,
    freed: Arc<watch::Sender<u64>>,
    /// A start was refused at [`LIFETIME_PROCESSES`]; retried once a permit
    /// is released.
    blocked: bool,
    tasks: JoinSet<()>,
    /// Limits warnings of processes not started at [`LIFETIME_PROCESSES`].
    busy: Warnings,
    /// Limits warnings of stdin updates dropped from a full queue.
    dropped: Warnings,
    /// Limits warnings of processes stopped for not reading stdin.
    stalled: Warnings,
}

/// What woke a worker.
enum Wake {
    Msg(Msg),
    Exit(Result<(task::Id, ()), JoinError>),
    Retry,
    Stop,
}

impl Worker {
    /// Handles changes until shutdown, then stops every process and reaps it.
    async fn run(mut self) {
        let mut slots = BTreeMap::new();
        let mut wake = None;
        loop {
            let event = tokio::select! {
                biased;
                () = self.stop.cancelled() => Wake::Stop,
                Some(exit) = self.tasks.join_next_with_id() => Wake::Exit(exit),
                msg = self.entry.mailbox.pop() => match msg {
                    Some(msg) => Wake::Msg(msg),
                    None => Wake::Stop,
                },
                freed = self.freed_rx.changed(), if self.blocked => match freed {
                    Ok(()) => Wake::Retry,
                    // Every process holds the sender, so it is never closed
                    // while a process could still release a permit.
                    Err(_closed) => Wake::Stop,
                },
                () = sleep_until(wake) => Wake::Retry,
            };
            match event {
                Wake::Stop => break,
                Wake::Retry => {}
                Wake::Exit(exit) => {
                    let task = self.reaped(exit);
                    self.exited(&mut slots, task);
                }
                Wake::Msg(Msg::Change { payload, arm }) => self.change(&mut slots, payload, arm),
                Wake::Msg(Msg::End(id)) => {
                    if let Some(mut proc) = slots.remove(&id).and_then(|slot| slot.proc)
                        && !proc.stopping
                    {
                        proc.close(hide_line(id));
                    }
                }
            }
            self.blocked = false;
            let now = Instant::now();
            wake = slots
                .iter_mut()
                .filter_map(|(id, slot)| self.reconcile(*id, slot, now))
                .min();
        }
        self.entry.mailbox.close();
        self.stop.cancel();
        while let Some(exit) = self.tasks.join_next_with_id().await {
            self.reaped(exit);
        }
    }

    /// Returns the task of a process that ended. A task that failed is
    /// logged, and since it never reported the permit it released, other
    /// hooks waiting for one are woken.
    fn reaped(&self, exit: Result<(task::Id, ()), JoinError>) -> task::Id {
        match exit {
            Ok((task, ())) => task,
            Err(error) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "hook process task failed"
                );
                self.freed
                    .send_modify(|count| *count = count.wrapping_add(1));
                error.id()
            }
        }
    }

    fn change(&mut self, slots: &mut BTreeMap<RequestId, Slot>, payload: Arc<Payload>, arm: bool) {
        let id = payload.id;
        let on_change = self.entry.lifetime.on_change;
        let slot = match slots.entry(id) {
            MapEntry::Occupied(slot) => slot.into_mut(),
            MapEntry::Vacant(slot) if arm => slot.insert(Slot {
                payload: Arc::clone(&payload),
                proc: None,
                started: None,
                last_start: None,
                dismissed: false,
            }),
            MapEntry::Vacant(_) => return,
        };
        let changed = slot
            .started
            .as_ref()
            .is_none_or(|started| started.changed(&payload));
        if arm || (changed && on_change != OnChange::Ignore) {
            slot.dismissed = false;
        }
        if let Some(proc) = slot.proc.as_mut().filter(|proc| !proc.stopping) {
            if !payload.shown {
                tracing::debug!(id = %id, "prompt suppressed; stopping the hook command");
                proc.close(hide_line(id));
            } else if on_change == OnChange::Stream {
                if self.feed(proc, payload.line(Op::Update)) == Pushed::Stalled {
                    if let Some(suppressed) = self.stalled.check() {
                        tracing::warn!(
                            id = %id,
                            limit = STDIN_QUEUE,
                            suppressed,
                            "hook command does not read stdin; stopping it"
                        );
                    }
                    proc.kill();
                    proc.stalled = true;
                } else {
                    // The process got these values on stdin.
                    slot.started = Some(Arc::clone(&payload));
                }
            }
        }
        slot.payload = payload;
    }

    /// Returns whether `slot` holds values its process has not been given,
    /// which start a new process once the old one ended.
    fn pending(&self, slot: &Slot) -> bool {
        self.entry.lifetime.on_change != OnChange::Ignore
            && slot
                .started
                .as_ref()
                .is_none_or(|started| started.changed(&slot.payload))
    }

    /// Starts, restarts or stops the process of request `id` to match its
    /// payload, and returns when to look again.
    fn reconcile(&mut self, id: RequestId, slot: &mut Slot, now: Instant) -> Option<Instant> {
        if slot.dismissed || !slot.payload.shown {
            return None;
        }
        let interval = Duration::from_millis(self.entry.lifetime.restart_interval_ms);
        let due = slot
            .last_start
            .map_or(now, |last| last.checked_add(interval).unwrap_or(now));
        match &mut slot.proc {
            Some(proc) if proc.stopping => None,
            Some(proc) => {
                let stale = self.entry.lifetime.on_change == OnChange::Restart
                    && slot
                        .started
                        .as_ref()
                        .is_none_or(|started| started.changed(&slot.payload));
                if !stale {
                    None
                } else if due > now {
                    Some(due)
                } else {
                    tracing::debug!(id = %id, "values changed; restarting the hook command");
                    proc.kill();
                    None
                }
            }
            None if due > now => Some(due),
            None => {
                let op = if slot.last_start.is_none() {
                    Op::Show
                } else {
                    Op::Update
                };
                slot.proc = self.start(id, op, &slot.payload);
                if slot.proc.is_some() {
                    slot.started = Some(Arc::clone(&slot.payload));
                    slot.last_start = Some(now);
                }
                None
            }
        }
    }

    /// Records the end of the process of `task`. One that ended without
    /// being told to is dismissed, unless its request changed since it was
    /// given its values; one stopped for not reading stdin is treated the
    /// same.
    fn exited(&self, slots: &mut BTreeMap<RequestId, Slot>, task: task::Id) {
        let slot = slots
            .values_mut()
            .find(|slot| slot.proc.as_ref().is_some_and(|proc| proc.task == task));
        let Some(slot) = slot else {
            return;
        };
        let stopped = slot
            .proc
            .take()
            .is_some_and(|proc| proc.stopping && !proc.stalled);
        if stopped {
            return;
        }
        if self.pending(slot) {
            tracing::debug!(id = %slot.payload.id, "hook command ended; starting it with newer values");
        } else {
            tracing::debug!(id = %slot.payload.id, "hook command ended before its request; dismissed");
            slot.dismissed = true;
        }
    }

    /// Queues `line` for the stdin of `proc` and returns how it was queued.
    fn feed(&self, proc: &Proc, line: Line) -> Pushed {
        let pushed = proc.lines.push(line);
        if pushed == Pushed::Dropped
            && let Some(suppressed) = self.dropped.check()
        {
            tracing::warn!(
                limit = STDIN_QUEUE,
                suppressed,
                "hook stdin queue full; dropping an update"
            );
        }
        pushed
    }

    /// Starts a process for request `id` with the environment of `op` and
    /// `payload` and the line of `op` queued for its stdin; `None` when
    /// [`LIFETIME_PROCESSES`] run.
    fn start(&mut self, id: RequestId, op: Op, payload: &Payload) -> Option<Proc> {
        // A permit released after this is seen by `changed`.
        self.freed_rx.mark_unchanged();
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                self.blocked = true;
                if let Some(suppressed) = self.busy.check() {
                    tracing::warn!(
                        limit = LIFETIME_PROCESSES,
                        suppressed,
                        "too many hook processes; starting one once another ended"
                    );
                }
                return None;
            }
            Err(error @ TryAcquireError::Closed) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "hook process permits closed"
                );
                return None;
            }
        };
        let lines = Arc::new(Queue::new(STDIN_QUEUE));
        lines.push(payload.line(op));
        let stop = self.stop.child_token();
        let closing = CancellationToken::new();
        let run = Run {
            command: self.entry.hook.command.clone(),
            lifetime: self.entry.lifetime,
            env: payload.env(op),
            lines: Arc::clone(&lines),
            stop: stop.clone(),
            closing: closing.clone(),
            failures: Arc::clone(&self.entry.failures),
            freed: Arc::clone(&self.freed),
            _permit: permit,
        };
        let span =
            tracing::info_span!("hook", hook = self.entry.index, id = id.0, op = op.as_str());
        let task = self.tasks.spawn(run.run().instrument(span)).id();
        Some(Proc {
            task,
            lines,
            stop,
            closing,
            stopping: false,
            stalled: false,
        })
    }
}

/// One process run, owned by its task.
struct Run {
    command: Vec<String>,
    lifetime: Lifetime,
    env: Vec<(String, String)>,
    lines: Arc<Queue<Line>>,
    stop: CancellationToken,
    closing: CancellationToken,
    failures: Arc<Warnings>,
    freed: Arc<watch::Sender<u64>>,
    _permit: OwnedSemaphorePermit,
}

impl Run {
    /// Runs the process until it exits or is stopped, writing its queued
    /// lines to its stdin, releases its permit and logs how it ended.
    ///
    /// The process output is not logged: it may echo what the user typed.
    async fn run(self) {
        let started = Instant::now();
        let Self {
            command,
            lifetime,
            env,
            lines,
            stop,
            closing,
            failures,
            freed,
            _permit: permit,
        } = self;
        let how = Stop {
            signal: lifetime.stop_signal,
            grace: Duration::from_millis(lifetime.stop_grace_ms),
            kill_group_on_exit: true,
        };
        tracing::debug!("starting hook command");
        let result = platform::supervised(&command, &env, true, how, |stdin| {
            write_lines(stdin, lines, stop, closing)
        })
        .await;
        drop(permit);
        freed.send_modify(|count| *count = count.wrapping_add(1));
        let elapsed_ms = millis(started.elapsed());
        match &result {
            Ok(finished) => {
                let reason = match finished.ending {
                    Ending::Exited => "exited",
                    Ending::TimedOut | Ending::Killed => "stopped",
                };
                tracing::debug!(
                    reason,
                    code = finished.code,
                    signal = finished.signal,
                    elapsed_ms,
                    "hook command ended"
                );
            }
            Err(error) => {
                if let Some(suppressed) = failures.check() {
                    tracing::warn!(
                        error = error as &dyn std::error::Error,
                        suppressed,
                        "cannot start the hook command"
                    );
                } else {
                    tracing::debug!(
                        error = error as &dyn std::error::Error,
                        "cannot start the hook command"
                    );
                }
            }
        }
    }
}

/// Writes `lines` to `stdin` until `stop` is cancelled or the queue is
/// closed and empty, then closes stdin.
///
/// Once `closing` is cancelled the remaining lines get [`CLOSE_FLUSH`]. A
/// failed write, such as to a process that closed its stdin, is logged
/// once and the later lines are discarded.
async fn write_lines(
    stdin: Option<Stdin>,
    lines: Arc<Queue<Line>>,
    stop: CancellationToken,
    closing: CancellationToken,
) -> Ending {
    let mut pipe = stdin;
    let mut deadline = None;
    loop {
        let line = tokio::select! {
            biased;
            () = stop.cancelled() => break,
            line = lines.pop() => line,
        };
        let Some(line) = line else {
            break;
        };
        let Some(writer) = pipe.as_mut() else {
            continue;
        };
        let mut write = std::pin::pin!(writer.write_all(line.text.as_bytes()));
        let written: Option<io::Result<()>> = loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => break None,
                () = sleep_until(deadline) => break None,
                () = closing.cancelled(), if deadline.is_none() => {
                    deadline = Instant::now().checked_add(CLOSE_FLUSH);
                }
                written = &mut write => break Some(written),
            }
        };
        match written {
            None => break,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "hook command stdin closed; discarding its lines"
                );
                pipe = None;
            }
        }
    }
    // Returning drops `pipe`, which closes stdin before the stop signal.
    Ending::Killed
}

/// Waits until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::{Path, PathBuf};

    use touchcue_core::StopSignal;

    use super::*;
    use crate::runner::WARN_INTERVAL;
    use crate::{HookSender, Hooks, HooksError, QUEUE};

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] io::Error),
        #[error(transparent)]
        Hooks(#[from] HooksError),
        #[error(transparent)]
        Int(#[from] std::num::ParseIntError),
        #[error(transparent)]
        Json(#[from] serde_json::Error),
        #[error("{0}")]
        Unexpected(String),
    }

    type TestResult = Result<(), TestError>;

    /// Time [`Hooks::shutdown`] waits before ending commands.
    const DRAIN: Duration = Duration::from_secs(2);

    /// A hook with `until` on `started` running `script` with `sh -c`, with
    /// `path` as `$0`; the product never adds a shell, the tests use one to
    /// observe the process.
    fn hook(script: &str, path: &Path, on_change: OnChange) -> Hook {
        Hook {
            on: vec![HookEvent::Started],
            matches: BTreeMap::new(),
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                path.display().to_string(),
            ],
            timeout_ms: 5000,
            concurrency: 4,
            lifetime: Some(Lifetime {
                on_change,
                stop_signal: StopSignal::Term,
                stop_grace_ms: 1000,
                restart_interval_ms: 100,
            }),
        }
    }

    fn restart(script: &str, path: &Path) -> Hook {
        hook(script, path, OnChange::Restart)
    }

    /// A hook stopped by `SIGHUP`, which `script` ignores, so that it reads
    /// its stdin to the end; the stop signal follows the hide line at once.
    /// It creates `$0.ready` once `SIGHUP` is ignored.
    fn reader(script: &str, path: &Path, on_change: OnChange) -> Hook {
        with_lifetime(
            hook(
                &format!("trap '' HUP; : > \"$0.ready\"; {script}"),
                path,
                on_change,
            ),
            |l| l.stop_signal = StopSignal::Hup,
        )
    }

    /// Waits until the process of [`reader`] writing `path` ignores `SIGHUP`.
    async fn wait_ready(path: &Path) -> TestResult {
        let ready = PathBuf::from(format!("{}.ready", path.display()));
        for _ in 0..500 {
            if std::fs::exists(&ready)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(format!(
            "{} not ready",
            path.display()
        )))
    }

    fn with_lifetime(mut hook: Hook, edit: impl FnOnce(&mut Lifetime)) -> Hook {
        if let Some(lifetime) = hook.lifetime.as_mut() {
            edit(lifetime);
        }
        hook
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn app(name: &str) -> BTreeMap<String, String> {
        values(&[("app.name", name)])
    }

    const PROMPT: Option<PromptText<'static>> = Some(PromptText {
        title: "t",
        body: "b",
        state: RequestState::Waiting,
    });

    fn show(sender: &HookSender, id: u64, values: &BTreeMap<String, String>) {
        sender.request(RequestId(id), &[HookEvent::Started], values, PROMPT);
    }

    fn update(sender: &HookSender, id: u64, values: &BTreeMap<String, String>) {
        sender.request(RequestId(id), &[HookEvent::Updated], values, PROMPT);
    }

    fn end(sender: &HookSender, id: u64) {
        sender.request(RequestId(id), &[HookEvent::Ended], &BTreeMap::new(), None);
    }

    /// Returns the lines of `path`, or none when it does not exist.
    fn lines(path: &Path) -> Result<Vec<String>, TestError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Waits until `path` has `count` lines.
    async fn wait_for_lines(path: &Path, count: usize) -> Result<Vec<String>, TestError> {
        for _ in 0..500 {
            let lines = lines(path)?;
            if lines.len() >= count {
                return Ok(lines);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(format!(
            "{} has fewer than {count} lines: {:?}",
            path.display(),
            lines(path)?
        )))
    }

    /// Waits until the last line of `path` contains `last`.
    async fn wait_for_last(path: &Path, last: &str) -> Result<Vec<String>, TestError> {
        for _ in 0..1000 {
            let lines = lines(path)?;
            if lines.last().is_some_and(|line| line.contains(last)) {
                return Ok(lines);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(format!(
            "{} does not end with {last}",
            path.display()
        )))
    }

    async fn first_pid(path: &Path) -> Result<u32, TestError> {
        Ok(wait_for_lines(path, 1)
            .await?
            .first()
            .ok_or_else(|| TestError::Unexpected("no pid".to_owned()))?
            .parse()?)
    }

    /// Returns whether process `pid` is gone, reaped by its parent.
    fn gone(pid: u32) -> Result<bool, TestError> {
        match std::fs::metadata(format!("/proc/{pid}")) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    async fn wait_gone(pid: u32) -> TestResult {
        for _ in 0..500 {
            if gone(pid)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(format!("process {pid} survived")))
    }

    fn file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    fn json(line: &str) -> Result<serde_json::Value, TestError> {
        Ok(serde_json::from_str(line)?)
    }

    fn op_id(line: &str) -> Result<(String, u64), TestError> {
        let value = json(line)?;
        let op = value["op"].as_str().unwrap_or_default().to_owned();
        let id = value["id"].as_u64().unwrap_or_default();
        Ok((op, id))
    }

    #[tokio::test]
    async fn prompt_reaches_env_and_stdin_sanitized_and_never_argv() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "out");
        // An ignored SIGHUP survives `exec`, so `cat` reads up to the end
        // of its stdin before the process exits.
        let script = "trap '' HUP; printf '%s\\n' \"$0\" \"$@\" > \"$0.args\"; env > \"$0.env\"; exec cat > \"$0.stdin\"";
        let mut config = with_lifetime(restart(script, &out), |l| {
            l.stop_signal = StopSignal::Hup;
        });
        config.command.push("--flag".to_owned());
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        let hostile = "-rf \"x\"\n<b>\u{1F600}</b>\u{202E}";
        let shown = values(&[
            ("app.name", hostile),
            ("app.exe", "/usr/bin/secret"),
            ("process.cmdline", "app --token secret"),
            ("request.state", "waiting"),
        ]);
        let body = format!("{hostile} waits");
        sender.request(
            RequestId(7),
            &[HookEvent::Started, HookEvent::Waiting],
            &shown,
            Some(PromptText {
                title: "Touch",
                body: &body,
                state: RequestState::Waiting,
            }),
        );
        let env_path = dir.path().join("out.env");
        wait_for_lines(&env_path, 1).await?;
        end(&sender, 7);
        let stdin = wait_for_lines(&dir.path().join("out.stdin"), 2).await?;
        hooks.shutdown(DRAIN).await?;

        let clean = "-rf \"x\" <b>\u{1F600}</b>";
        let args = lines(&dir.path().join("out.args"))?;
        assert_eq!(args, [out.display().to_string(), "--flag".to_owned()]);
        let env = lines(&env_path)?;
        let ours: Vec<&str> = env
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("TOUCHCUE_"))
            .collect();
        let body_var = format!("TOUCHCUE_BODY={clean} waits");
        let app_var = format!("TOUCHCUE_APP_NAME={clean}");
        let expected = [
            "TOUCHCUE_EVENT=show",
            "TOUCHCUE_TITLE=Touch",
            body_var.as_str(),
            app_var.as_str(),
            "TOUCHCUE_REQUEST_STATE=waiting",
        ];
        assert_eq!(ours.len(), expected.len(), "{ours:?}");
        for line in expected {
            assert!(ours.contains(&line), "{line} missing from {ours:?}");
        }

        let [show, hide] = stdin.as_slice() else {
            return Err(TestError::Unexpected(format!("{stdin:?}")));
        };
        assert_eq!(
            json(show)?,
            serde_json::json!({
                "op": "show",
                "id": 7,
                "title": "Touch",
                "body": format!("{clean} waits"),
                "values": { "app.name": clean, "request.state": "waiting" },
            })
        );
        assert_eq!(json(hide)?, serde_json::json!({ "op": "hide", "id": 7 }));
        Ok(())
    }

    #[tokio::test]
    async fn changed_values_restart_at_most_once_per_interval() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let config = with_lifetime(
            restart(
                "echo \"$TOUCHCUE_EVENT $TOUCHCUE_APP_NAME\" >> \"$0\"; exec sleep 30",
                &out,
            ),
            |l| l.restart_interval_ms = 400,
        );
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        let shown = Instant::now();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        update(&sender, 1, &app("b"));
        update(&sender, 1, &app("c"));
        let starts = wait_for_lines(&out, 2).await?;
        assert!(shown.elapsed() >= Duration::from_millis(400));
        assert_eq!(starts, ["show a", "update c"]);

        let mut elapsed = app("c");
        elapsed.insert(ELAPSED_KEY.to_owned(), "5".to_owned());
        update(&sender, 1, &elapsed);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(lines(&out)?.len(), 2, "an elapsed-only change restarted");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn only_armed_requests_start_and_suppression_stops() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let hooks = Hooks::spawn(vec![restart(
            "echo \"$TOUCHCUE_EVENT $$\" >> \"$0\"; exec sleep 30",
            &out,
        )]);
        let sender = hooks.sender();
        // An update of a request whose start was not seen arms nothing.
        update(&sender, 2, &app("a"));
        // A request suppressed from its start is armed but not started.
        sender.request(RequestId(1), &[HookEvent::Started], &app("a"), None);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&out)?, Vec::<String>::new());
        update(&sender, 1, &app("a"));
        let first = wait_for_lines(&out, 1).await?;
        let pid: u32 = first
            .first()
            .and_then(|line| line.strip_prefix("show "))
            .ok_or_else(|| TestError::Unexpected(format!("{first:?}")))?
            .parse()?;
        sender.request(RequestId(1), &[HookEvent::Updated], &app("a"), None);
        wait_gone(pid).await?;
        update(&sender, 1, &app("a"));
        let starts = wait_for_lines(&out, 2).await?;
        assert!(
            starts
                .get(1)
                .is_some_and(|line| line.starts_with("update ")),
            "{starts:?}"
        );
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn exited_process_is_dismissed_and_its_group_killed() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let background = file(&dir, "background");
        let script = format!(
            "sleep 30 & echo $! >> '{}'; echo \"$TOUCHCUE_EVENT $TOUCHCUE_APP_NAME\" >> \"$0\"; exit 2",
            background.display()
        );
        let hooks = Hooks::spawn(vec![restart(&script, &out)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        wait_gone(first_pid(&background).await?).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut same = app("a");
        same.insert(ELAPSED_KEY.to_owned(), "3".to_owned());
        update(&sender, 1, &same);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&out)?, ["show a"]);
        update(&sender, 1, &app("b"));
        wait_for_lines(&out, 2).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(lines(&out)?, ["show a", "update b"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn failed_start_is_dismissed_without_a_retry_loop() -> TestResult {
        let mut unstartable = restart("", Path::new("/"));
        unstartable.command = vec!["/nonexistent/touchcue-hook".to_owned()];
        let hooks = Hooks::spawn(vec![unstartable]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let mut same = app("a");
            same.insert(ELAPSED_KEY.to_owned(), "1".to_owned());
            update(&sender, 1, &same);
        }
        let failures = sender
            .lifetimes
            .entries
            .first()
            .map(|entry| Arc::clone(&entry.failures))
            .ok_or_else(|| TestError::Unexpected("no hook".to_owned()))?;
        // The one failure was warned about; this check is held back.
        assert_eq!(failures.check(), None);
        tokio::time::advance(WARN_INTERVAL).await;
        assert_eq!(failures.check(), Some(1), "the start was retried");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn end_during_a_restart_starts_nothing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let config = with_lifetime(
            restart(
                "trap '' TERM; echo $$ >> \"$0\"; while :; do sleep 0.05; done",
                &out,
            ),
            |l| {
                l.stop_grace_ms = 300;
                l.restart_interval_ms = 0;
            },
        );
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        let pid = first_pid(&out).await?;
        update(&sender, 1, &app("b"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        end(&sender, 1);
        wait_gone(pid).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&out)?.len(), 1, "a process started after the end");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn stream_gets_every_change_in_order() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "stream");
        let hooks = Hooks::spawn(vec![reader("exec cat >> \"$0\"", &out, OnChange::Stream)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_ready(&out).await?;
        update(&sender, 1, &app("b"));
        update(&sender, 1, &app("c"));
        end(&sender, 1);
        let got = wait_for_lines(&out, 4).await?;
        let ops = got
            .iter()
            .map(|line| op_id(line))
            .collect::<Result<Vec<_>, _>>()?;
        let expected = [("show", 1), ("update", 1), ("update", 1), ("hide", 1)]
            .map(|(op, id)| (op.to_owned(), id));
        assert_eq!(ops, expected);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn ignore_keeps_the_process_without_updates() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "ignore");
        let hooks = Hooks::spawn(vec![reader("exec cat >> \"$0\"", &out, OnChange::Ignore)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_ready(&out).await?;
        update(&sender, 1, &app("b"));
        end(&sender, 1);
        let got = wait_for_last(&out, "hide").await?;
        let ops = got
            .iter()
            .map(|line| op_id(line))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ops, [("show".to_owned(), 1), ("hide".to_owned(), 1)]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn slow_reader_drops_updates_but_keeps_hide() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "stream");
        let hooks = Hooks::spawn(vec![reader(
            "sleep 1; exec cat >> \"$0\"",
            &out,
            OnChange::Stream,
        )]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_ready(&out).await?;
        for count in 0..800 {
            let count = count.to_string();
            update(
                &sender,
                1,
                &values(&[("app.name", "a"), ("request.count", &count)]),
            );
        }
        end(&sender, 1);
        let got = wait_for_last(&out, "hide").await?;
        assert!(got.len() < 802, "no update was dropped");
        assert_eq!(
            op_id(got.first().map_or("", String::as_str))?,
            ("show".to_owned(), 1)
        );
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    /// Sends updates of request 1 in batches, each value from `value`,
    /// until the process `pid` is gone; returns the last value sent.
    async fn stall(
        sender: &HookSender,
        pid: u32,
        value: impl Fn(usize) -> BTreeMap<String, String>,
    ) -> Result<usize, TestError> {
        let mut sent = 0;
        for _ in 0..200 {
            for _ in 0..100 {
                sent += 1;
                update(sender, 1, &value(sent));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            if gone(pid)? {
                return Ok(sent);
            }
        }
        Err(TestError::Unexpected(format!("process {pid} kept running")))
    }

    /// A stream hook whose process logs `$TOUCHCUE_EVENT $TOUCHCUE_REQUEST_COUNT $$`
    /// and never reads stdin.
    fn never_reads(path: &Path) -> Hook {
        with_lifetime(
            hook(
                "echo \"$TOUCHCUE_EVENT $TOUCHCUE_REQUEST_COUNT $$\" >> \"$0\"; exec sleep 30",
                path,
                OnChange::Stream,
            ),
            |l| l.restart_interval_ms = 1500,
        )
    }

    fn pid_of(line: Option<&String>) -> Result<u32, TestError> {
        let pid = line.and_then(|line| line.rsplit(' ').next());
        Ok(pid
            .ok_or_else(|| TestError::Unexpected("no pid".to_owned()))?
            .parse()?)
    }

    #[tokio::test]
    async fn stalled_consumer_is_stopped_and_restarted_with_newer_values() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let hooks = Hooks::spawn(vec![never_reads(&out)]);
        let sender = hooks.sender();
        let filler = "x".repeat(900);
        let value = |count: usize| {
            let count = count.to_string();
            values(&[
                ("app.name", "a"),
                ("request.count", &count),
                ("request.detail", &filler),
            ])
        };
        show(&sender, 1, &value(0));
        let pid = pid_of(wait_for_lines(&out, 1).await?.first())?;
        let last = stall(&sender, pid, value).await?;
        // The restart waits for `restart_interval_ms` after the first start,
        // which is after the last update was sent.
        let starts = wait_for_lines(&out, 2).await?;
        let second = starts.get(1).map_or("", String::as_str);
        assert!(second.starts_with(&format!("update {last} ")), "{starts:?}");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn stalled_consumer_with_elapsed_only_updates_is_dismissed() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let config = with_lifetime(never_reads(&out), |l| l.restart_interval_ms = 0);
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        let filler = "x".repeat(900);
        let value = |elapsed: usize| {
            let elapsed = elapsed.to_string();
            values(&[
                ("app.name", "a"),
                ("request.count", "1"),
                ("request.detail", &filler),
                (ELAPSED_KEY, &elapsed),
            ])
        };
        show(&sender, 1, &value(0));
        let pid = pid_of(wait_for_lines(&out, 1).await?.first())?;
        stall(&sender, pid, value).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&out)?.len(), 1, "a dismissed process started again");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn stop_signal_is_honoured() -> TestResult {
        let dir = tempfile::tempdir()?;
        let script = "trap 'echo int >> \"$0\"; exit 0' INT; trap 'echo term >> \"$0\"; exit 0' TERM; echo ready >> \"$0\"; while :; do sleep 0.05; done";
        let int = file(&dir, "int");
        let term = file(&dir, "term");
        let by_int = with_lifetime(restart(script, &int), |l| l.stop_signal = StopSignal::Int);
        let hooks = Hooks::spawn(vec![by_int, restart(script, &term)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&int, 1).await?;
        wait_for_lines(&term, 1).await?;
        end(&sender, 1);
        assert_eq!(wait_for_lines(&int, 2).await?, ["ready", "int"]);
        assert_eq!(wait_for_lines(&term, 2).await?, ["ready", "term"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn ignored_stop_signal_ends_in_sigkill_and_reaping() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "pid");
        let config = with_lifetime(
            restart(
                "trap '' TERM; echo $$ >> \"$0\"; while :; do sleep 0.05; done",
                &out,
            ),
            |l| l.stop_grace_ms = 300,
        );
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        let pid = first_pid(&out).await?;
        let ended = Instant::now();
        end(&sender, 1);
        wait_gone(pid).await?;
        assert!(ended.elapsed() >= Duration::from_millis(300));
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_stops_processes_ignoring_signals() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "pid");
        let config = with_lifetime(
            hook(
                "trap '' TERM; echo $$ >> \"$0\"; exec sleep 30",
                &out,
                OnChange::Stream,
            ),
            |l| l.stop_grace_ms = 200,
        );
        let hooks = Hooks::spawn(vec![config]);
        show(&hooks.sender(), 1, &app("a"));
        let pid = first_pid(&out).await?;
        let stopping = Instant::now();
        hooks.shutdown(DRAIN).await?;
        assert!(stopping.elapsed() >= Duration::from_millis(200));
        assert!(gone(pid)?, "the process was not reaped");
        Ok(())
    }

    #[tokio::test]
    async fn refused_start_runs_once_another_hook_frees_a_slot() -> TestResult {
        let dir = tempfile::tempdir()?;
        let first = file(&dir, "first");
        let second = file(&dir, "second");
        let script = "echo started >> \"$0\"; exec sleep 30";
        let mut only_a = restart(script, &first);
        only_a.matches = app("a");
        let mut only_b = restart(script, &second);
        only_b.matches = app("b");
        let hooks = Hooks::with_limits(vec![only_a, only_b], QUEUE, 1);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&first, 1).await?;
        show(&sender, 2, &app("b"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&second)?.len(), 0, "the limit was exceeded");
        // Ending request 1 sends hook B nothing; the freed slot wakes it.
        end(&sender, 1);
        wait_for_lines(&second, 1).await?;
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    fn suppress(sender: &HookSender, id: u64, values: &BTreeMap<String, String>) {
        sender.request(RequestId(id), &[HookEvent::Updated], values, None);
    }

    /// Logs `$TOUCHCUE_EVENT $TOUCHCUE_APP_NAME` to `$0`, then runs `rest`.
    fn logging(rest: &str) -> String {
        format!("echo \"$TOUCHCUE_EVENT $TOUCHCUE_APP_NAME\" >> \"$0\"; {rest}")
    }

    #[tokio::test]
    async fn change_before_an_exit_restarts_with_the_newest_values() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let config = with_lifetime(restart(&logging("sleep 0.2"), &out), |l| {
            l.restart_interval_ms = 500;
        });
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        update(&sender, 1, &app("b"));
        update(&sender, 1, &app("c"));
        // The process exits before the restart is due; the change still counts.
        assert_eq!(wait_for_lines(&out, 2).await?, ["show a", "update c"]);
        // The second process exits with nothing newer, so it stays dismissed.
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&out)?.len(), 2);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn waiting_rearms_a_dismissed_hook_on_revival() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let mut config = restart(&logging("exit 0"), &out);
        config.on = vec![HookEvent::Waiting];
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        let a = app("a");
        sender.request(
            RequestId(1),
            &[HookEvent::Started, HookEvent::Waiting],
            &a,
            PROMPT,
        );
        wait_for_lines(&out, 1).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        update(&sender, 1, &a);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&out)?, ["show a"]);
        let revived = [HookEvent::Updated, HookEvent::Revived, HookEvent::Waiting];
        sender.request(RequestId(1), &revived, &a, PROMPT);
        assert_eq!(wait_for_lines(&out, 2).await?, ["show a", "update a"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn started_does_not_rearm_and_ignore_stays_dismissed() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let hooks = Hooks::spawn(vec![hook(&logging("exit 0"), &out, OnChange::Ignore)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let revived = [HookEvent::Updated, HookEvent::Revived, HookEvent::Waiting];
        sender.request(RequestId(1), &revived, &app("b"), PROMPT);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(lines(&out)?, ["show a"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn match_gates_which_requests_arm_the_hook() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let mut config = restart(&logging("exec sleep 30"), &out);
        config.matches = app("a");
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        show(&sender, 1, &app("b"));
        // The values match later, but no start event fires with them.
        update(&sender, 1, &app("a"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lines(&out)?, Vec::<String>::new());
        show(&sender, 2, &app("a"));
        assert_eq!(wait_for_lines(&out, 1).await?, ["show a"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn later_suppression_writes_hide() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "stdin");
        let hooks = Hooks::spawn(vec![reader("exec cat >> \"$0\"", &out, OnChange::Restart)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_ready(&out).await?;
        suppress(&sender, 1, &app("a"));
        let got = wait_for_last(&out, "hide").await?;
        let ops = got
            .iter()
            .map(|line| op_id(line))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(ops, [("show".to_owned(), 1), ("hide".to_owned(), 1)]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn unsuppressing_keeps_a_dismissal() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let hooks = Hooks::spawn(vec![restart(&logging("exit 0"), &out)]);
        let sender = hooks.sender();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        suppress(&sender, 1, &app("a"));
        update(&sender, 1, &app("a"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(lines(&out)?, ["show a"]);
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn suppression_during_a_pending_restart_waits_for_the_prompt() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let config = with_lifetime(restart(&logging("exec sleep 30"), &out), |l| {
            l.restart_interval_ms = 1000;
        });
        let hooks = Hooks::spawn(vec![config]);
        let sender = hooks.sender();
        let shown = Instant::now();
        show(&sender, 1, &app("a"));
        wait_for_lines(&out, 1).await?;
        update(&sender, 1, &app("b"));
        suppress(&sender, 1, &app("b"));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(lines(&out)?, ["show a"], "started while suppressed");
        update(&sender, 1, &app("b"));
        assert_eq!(wait_for_lines(&out, 2).await?, ["show a", "update b"]);
        assert!(shown.elapsed() >= Duration::from_millis(1000));
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    /// Sends the cancel of request 1 as the daemon does: the state changes
    /// in the values and in the prompt.
    fn cancel(sender: &HookSender) {
        let cancelled = RequestState::Lingering(touchcue_core::EndReason::Cancelled);
        sender.request(
            RequestId(1),
            &[
                HookEvent::Updated,
                HookEvent::Lingering,
                HookEvent::Cancelled,
            ],
            &values(&[("app.name", "a"), ("request.state", "cancelled")]),
            Some(PromptText {
                title: "t",
                body: "b",
                state: cancelled,
            }),
        );
    }

    #[tokio::test]
    async fn cancel_restarts_with_the_body_the_popup_shows() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "starts");
        let hooks = Hooks::spawn(vec![restart(
            "echo \"$TOUCHCUE_EVENT $TOUCHCUE_BODY\" >> \"$0\"; exec sleep 30",
            &out,
        )]);
        let sender = hooks.sender();
        show(
            &sender,
            1,
            &values(&[("app.name", "a"), ("request.state", "waiting")]),
        );
        wait_for_lines(&out, 1).await?;
        cancel(&sender);
        assert_eq!(
            wait_for_lines(&out, 2).await?,
            ["show b", "update b (cancelled)"]
        );
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cancel_streams_the_body_the_popup_shows() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = file(&dir, "stream");
        let hooks = Hooks::spawn(vec![reader("exec cat >> \"$0\"", &out, OnChange::Stream)]);
        let sender = hooks.sender();
        show(
            &sender,
            1,
            &values(&[("app.name", "a"), ("request.state", "waiting")]),
        );
        wait_ready(&out).await?;
        cancel(&sender);
        end(&sender, 1);
        let got = wait_for_last(&out, "hide").await?;
        let update = got
            .iter()
            .find(|line| line.contains("\"update\""))
            .ok_or_else(|| TestError::Unexpected(format!("{got:?}")))?;
        let update = json(update)?;
        assert_eq!(update["body"], "b (cancelled)");
        assert_eq!(update["values"]["request.state"], "cancelled");
        hooks.shutdown(DRAIN).await?;
        Ok(())
    }
}
