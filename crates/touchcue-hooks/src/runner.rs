//! Queues, concurrency limits and shutdown of hook runs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;
use touchcue_core::text::sanitize;
use touchcue_core::{Hook, HookEvent, RateLimit};
use tracing::Instrument as _;

use crate::platform;

/// Events queued per hook; an event that finds its hook's queue full is
/// dropped for that hook.
pub const QUEUE: usize = 256;
/// Time between `SIGTERM` and `SIGKILL` when a command is ended.
pub const KILL_GRACE: Duration = Duration::from_secs(1);
/// Longest output logged per stream, in chars.
const OUTPUT_LOG_MAX: usize = 4096;
/// Longest wait for runs to end once they were told to: the kill grace plus
/// time to reap.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// Shortest interval between two warnings of one kind for one hook: a full
/// queue, or a run that failed.
const WARN_INTERVAL: Duration = Duration::from_secs(10);

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    expect(dead_code, reason = "only the Linux runner starts commands")
)]
pub(crate) enum Ending {
    /// The command exited on its own.
    Exited,
    /// The command ran past its timeout and was ended.
    TimedOut,
    /// The command was ended at shutdown.
    Killed,
}

/// Result of a run whose command was started and reaped.
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) ending: Ending,
    /// Exit code, when the command exited normally.
    pub(crate) code: Option<i32>,
    /// Signal that ended the command, if any.
    pub(crate) signal: Option<i32>,
    /// The first 4 KiB of stdout.
    pub(crate) stdout: Vec<u8>,
    /// The first 4 KiB of stderr.
    pub(crate) stderr: Vec<u8>,
}

/// One event queued for a hook.
#[derive(Debug)]
struct Job {
    event: HookEvent,
    env: Arc<[(String, String)]>,
}

#[derive(Debug)]
struct Queue {
    hook: Arc<Hook>,
    tx: mpsc::Sender<Job>,
    /// Limits the full-queue warnings of this hook across every sender.
    full: Warnings,
    /// Limits the failed-run warnings of this hook across its runs.
    failures: Arc<Warnings>,
}

/// Rate limit of one kind of repeating warning of one hook, on the Tokio clock.
#[derive(Debug)]
struct Warnings(Mutex<RateLimit>);

impl Warnings {
    fn new() -> Self {
        Self(Mutex::new(RateLimit::new(WARN_INTERVAL)))
    }

    /// Returns how many warnings were held back since the last one let
    /// through, or `None` when this one is held back.
    fn check(&self) -> Option<u64> {
        let now = tokio::time::Instant::now().into_std();
        let mut limit = match self.0.lock() {
            Ok(limit) => limit,
            // A panic while holding the lock cannot leave the limit inconsistent.
            Err(poisoned) => poisoned.into_inner(),
        };
        limit.check(now)
    }
}

/// Failure to stop the hook runs.
#[derive(Debug, thiserror::Error)]
pub enum HooksError {
    #[error("hook commands did not end within {KILL_WAIT:?} of being ended")]
    StopTimeout(#[source] tokio::time::error::Elapsed),
}

/// Running hooks: one task per hook that starts its runs.
///
/// A hook starts runs in the order its events were queued, at most
/// `concurrency` at once; nothing is ordered across hooks. Dropping it
/// ends every running command without waiting; [`Hooks::shutdown`] waits.
#[derive(Debug)]
pub struct Hooks {
    sender: HookSender,
    stop: CancellationToken,
    kill: CancellationToken,
    tracker: TaskTracker,
    _guard: DropGuard,
}

impl Hooks {
    /// Starts a task per hook. Must be called within a Tokio runtime with
    /// I/O and time enabled.
    #[must_use]
    pub fn spawn(hooks: Vec<Hook>) -> Self {
        Self::with_queue(hooks, QUEUE)
    }

    fn with_queue(hooks: Vec<Hook>, capacity: usize) -> Self {
        let stop = CancellationToken::new();
        let kill = CancellationToken::new();
        let tracker = TaskTracker::new();
        let queues: Arc<[Queue]> = hooks
            .into_iter()
            .enumerate()
            .map(|(index, hook)| {
                let (tx, rx) = mpsc::channel(capacity);
                let queue = Queue {
                    hook: Arc::new(hook),
                    tx,
                    full: Warnings::new(),
                    failures: Arc::new(Warnings::new()),
                };
                let worker = Worker {
                    index,
                    hook: Arc::clone(&queue.hook),
                    failures: Arc::clone(&queue.failures),
                    tracker: tracker.clone(),
                    stop: stop.clone(),
                    kill: kill.clone(),
                };
                tracker.spawn(
                    worker
                        .run(rx)
                        .instrument(tracing::info_span!("hooks", hook = index)),
                );
                queue
            })
            .collect();
        Self {
            sender: HookSender { queues },
            stop,
            _guard: kill.clone().drop_guard(),
            kill,
            tracker,
        }
    }

    /// Returns a sender that queues events for the hooks.
    #[must_use]
    pub fn sender(&self) -> HookSender {
        self.sender.clone()
    }

    /// Stops accepting events, waits up to `drain` for the queued and
    /// running runs, then ends the commands still running and drops the
    /// runs not started.
    ///
    /// # Errors
    ///
    /// Returns [`HooksError::StopTimeout`] when the ended commands are not
    /// reaped within 2 s.
    #[tracing::instrument(name = "hooks_shutdown", skip_all, err)]
    pub async fn shutdown(self, drain: Duration) -> Result<(), HooksError> {
        self.stop.cancel();
        self.tracker.close();
        match tokio::time::timeout(drain, self.tracker.wait()).await {
            Ok(()) => return Ok(()),
            Err(_elapsed) => tracing::warn!(
                drain_ms = millis(drain),
                "hook commands still running; ending them"
            ),
        }
        self.kill.cancel();
        tokio::time::timeout(KILL_WAIT, self.tracker.wait())
            .await
            .map_err(HooksError::StopTimeout)
    }
}

/// Queues events for the hooks without waiting.
#[derive(Debug, Clone)]
pub struct HookSender {
    queues: Arc<[Queue]>,
}

impl HookSender {
    /// Queues a run of every hook that `event` with placeholder `values`
    /// applies to, and returns how many were queued.
    ///
    /// A hook whose queue is full misses the newest event, with a warning
    /// rate-limited per hook; after [`Hooks::shutdown`] began, every hook
    /// misses it.
    pub fn fire(&self, event: HookEvent, values: &BTreeMap<String, String>) -> usize {
        let mut env = None;
        let mut queued = 0;
        for (index, queue) in self.queues.iter().enumerate() {
            if !queue.hook.applies(event, values) {
                continue;
            }
            let env = env.get_or_insert_with(|| Arc::<[_]>::from(crate::env(event, values)));
            let job = Job {
                event,
                env: Arc::clone(env),
            };
            match queue.tx.try_send(job) {
                Ok(()) => queued += 1,
                Err(TrySendError::Full(_)) => {
                    if let Some(suppressed) = queue.full.check() {
                        tracing::warn!(
                            hook = index,
                            event = event.as_str(),
                            limit = queue.tx.max_capacity(),
                            suppressed,
                            "hook queue full; dropping an event"
                        );
                    }
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::debug!(
                        hook = index,
                        event = event.as_str(),
                        "hooks stopped; dropping an event"
                    );
                }
            }
        }
        queued
    }
}

/// Starts the runs of one hook.
struct Worker {
    index: usize,
    hook: Arc<Hook>,
    failures: Arc<Warnings>,
    tracker: TaskTracker,
    stop: CancellationToken,
    kill: CancellationToken,
}

impl Worker {
    /// Starts a run per queued event once a concurrency permit is free,
    /// until the queue is closed and empty or `kill` is cancelled; `stop`
    /// closes the queue.
    async fn run(self, mut rx: mpsc::Receiver<Job>) {
        let permits = Arc::new(Semaphore::new(usize::from(self.hook.concurrency)));
        let timeout = Duration::from_millis(self.hook.timeout_ms);
        let mut stopping = false;
        let mut dropped: u64 = 0;
        loop {
            let job = tokio::select! {
                biased;
                () = self.kill.cancelled() => break,
                () = self.stop.cancelled(), if !stopping => {
                    rx.close();
                    stopping = true;
                    continue;
                }
                job = rx.recv() => match job {
                    Some(job) => job,
                    None => break,
                },
            };
            let permit = tokio::select! {
                biased;
                () = self.kill.cancelled() => {
                    dropped += 1;
                    break;
                }
                permit = Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(error) => {
                        tracing::error!(
                            error = &error as &dyn std::error::Error,
                            "hook permits closed"
                        );
                        break;
                    }
                },
            };
            self.tracker.spawn(run(
                self.index,
                Arc::clone(&self.hook),
                Arc::clone(&self.failures),
                job,
                timeout,
                self.kill.clone(),
                permit,
            ));
        }
        rx.close();
        // A closed queue yields its remaining events, then `None`.
        while let Some(_job) = rx.recv().await {
            dropped += 1;
        }
        if dropped > 0 {
            tracing::warn!(dropped, "hook runs not started before shutdown");
        }
    }
}

/// Runs the hook's command for `job` while holding `permit`, and logs how
/// it ended.
#[tracing::instrument(
    name = "hook",
    skip_all,
    fields(hook = index, event = job.event.as_str())
)]
async fn run(
    index: usize,
    hook: Arc<Hook>,
    failures: Arc<Warnings>,
    job: Job,
    timeout: Duration,
    kill: CancellationToken,
    permit: OwnedSemaphorePermit,
) {
    let started = Instant::now();
    let result = platform::run(&hook.command, &job.env, timeout, &kill).await;
    drop(permit);
    let elapsed_ms = millis(started.elapsed());
    let finished = match result {
        Ok(finished) => finished,
        Err(error) => {
            let failure = Failure {
                reason: "not_started",
                error: Some(&error),
                code: None,
                signal: None,
            };
            failure.log(&failures, elapsed_ms, hook.timeout_ms);
            return;
        }
    };
    log_output("stdout", &finished.stdout);
    log_output("stderr", &finished.stderr);
    let reason = match finished.ending {
        Ending::Exited if finished.code == Some(0) => {
            tracing::debug!(elapsed_ms, "hook command succeeded");
            return;
        }
        Ending::Exited => "failed",
        Ending::TimedOut => "timed_out",
        Ending::Killed => "ended_at_shutdown",
    };
    let failure = Failure {
        reason,
        error: None,
        code: finished.code,
        signal: finished.signal,
    };
    failure.log(&failures, elapsed_ms, hook.timeout_ms);
}

/// A run that did not succeed.
struct Failure<'a> {
    /// `not_started`, `failed`, `timed_out` or `ended_at_shutdown`.
    reason: &'static str,
    error: Option<&'a (dyn std::error::Error + 'static)>,
    code: Option<i32>,
    signal: Option<i32>,
}

impl Failure<'_> {
    /// Logs the failure as a warning rate-limited by `failures`, or at
    /// debug level when the warning is held back.
    fn log(&self, failures: &Warnings, elapsed_ms: u64, timeout_ms: u64) {
        let Self {
            reason,
            error,
            code,
            signal,
        } = *self;
        if let Some(suppressed) = failures.check() {
            tracing::warn!(
                reason,
                error,
                code,
                signal,
                elapsed_ms,
                timeout_ms,
                suppressed,
                "hook command failed"
            );
        } else {
            tracing::debug!(
                reason,
                error,
                code,
                signal,
                elapsed_ms,
                timeout_ms,
                "hook command failed"
            );
        }
    }
}

/// Logs a captured output stream at debug level as sanitized text.
fn log_output(stream: &'static str, bytes: &[u8]) {
    if let Some(text) = sanitize(&String::from_utf8_lossy(bytes), OUTPUT_LOG_MAX) {
        tracing::debug!(stream, output = %text, "hook command output");
    }
}

/// Returns `duration` in whole milliseconds, saturating at `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(duration.subsec_millis()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Hooks(#[from] HooksError),
        #[error(transparent)]
        Int(#[from] std::num::ParseIntError),
        #[error("{0}")]
        Unexpected(String),
    }

    type TestResult = Result<(), TestError>;

    /// A hook running `script` with `sh -c`, with `path` as `$0`; the
    /// product never uses a shell, the tests use one to observe the runs.
    fn hook(on: &[HookEvent], script: &str, path: &Path) -> Hook {
        Hook {
            on: on.to_vec(),
            matches: BTreeMap::new(),
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                path.display().to_string(),
            ],
            timeout_ms: 10_000,
            concurrency: 4,
        }
    }

    /// A hook on `started` whose program does not exist, so every run fails
    /// at once without a process, which keeps a paused clock paused.
    fn unstartable() -> Hook {
        Hook {
            on: vec![HookEvent::Started],
            matches: BTreeMap::new(),
            command: vec!["/nonexistent/touchcue-hook".to_owned()],
            timeout_ms: 10_000,
            concurrency: 4,
        }
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Returns the lines of `path`, or none when it does not exist.
    fn lines(path: &Path) -> Result<Vec<String>, TestError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
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
            "{} has fewer than {count} lines",
            path.display()
        )))
    }

    #[tokio::test]
    async fn env_reaches_the_command_and_shutdown_drains_the_queue() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("env");
        let hooks = Hooks::spawn(vec![hook(
            &[HookEvent::DaemonStopping],
            "env > \"$0\"",
            &out,
        )]);
        let sender = hooks.sender();
        let queued = sender.fire(
            HookEvent::DaemonStopping,
            &values(&[
                ("app.name", "Fire\u{202E}fox"),
                ("app.exe", "/usr/bin/firefox"),
                ("process.cmdline", "firefox --secret"),
                ("request.state", "waiting"),
            ]),
        );
        assert_eq!(queued, 1);
        hooks.shutdown(Duration::from_secs(10)).await?;
        let env = lines(&out)?;
        let ours: Vec<&str> = env
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("TOUCHCUE_"))
            .collect();
        assert_eq!(
            ours.len(),
            3,
            "only the event and the published values: {ours:?}"
        );
        for expected in [
            "TOUCHCUE_EVENT=daemon_stopping",
            "TOUCHCUE_APP_NAME=Fire fox",
            "TOUCHCUE_REQUEST_STATE=waiting",
        ] {
            assert!(ours.contains(&expected), "{expected} missing from {ours:?}");
        }
        assert_eq!(sender.fire(HookEvent::DaemonStopping, &BTreeMap::new()), 0);
        Ok(())
    }

    #[tokio::test]
    async fn only_listed_events_and_matching_values_run() -> TestResult {
        let dir = tempfile::tempdir()?;
        let mut ssh = hook(&[HookEvent::Started], "true", dir.path());
        ssh.matches = values(&[("process.name", "ssh")]);
        let hooks = Hooks::spawn(vec![
            ssh,
            hook(&[HookEvent::Started, HookEvent::Ended], "true", dir.path()),
        ]);
        let sender = hooks.sender();
        let from_ssh = values(&[("process.name", "ssh")]);
        assert_eq!(sender.fire(HookEvent::Started, &from_ssh), 2);
        assert_eq!(sender.fire(HookEvent::Ended, &from_ssh), 1);
        assert_eq!(sender.fire(HookEvent::Started, &BTreeMap::new()), 1);
        assert_eq!(sender.fire(HookEvent::Touched, &from_ssh), 0);
        hooks.shutdown(Duration::from_secs(10)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn runs_of_a_hook_start_in_event_order() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("order");
        let mut ordered = hook(
            &[HookEvent::Updated],
            "echo \"$TOUCHCUE_REQUEST_COUNT\" >> \"$0\"",
            &out,
        );
        ordered.concurrency = 1;
        let hooks = Hooks::spawn(vec![ordered]);
        let sender = hooks.sender();
        for count in 1..=10 {
            let count = count.to_string();
            sender.fire(HookEvent::Updated, &values(&[("request.count", &count)]));
        }
        hooks.shutdown(Duration::from_secs(10)).await?;
        let expected: Vec<String> = (1..=10).map(|n: u32| n.to_string()).collect();
        assert_eq!(lines(&out)?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn concurrency_limits_parallel_runs() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("runs");
        let mut limited = hook(
            &[HookEvent::Started],
            "echo start >> \"$0\"; sleep 0.3; echo end >> \"$0\"",
            &out,
        );
        limited.concurrency = 2;
        let hooks = Hooks::spawn(vec![limited]);
        let sender = hooks.sender();
        for _ in 0..5 {
            sender.fire(HookEvent::Started, &BTreeMap::new());
        }
        hooks.shutdown(Duration::from_secs(10)).await?;
        let mut running = 0i32;
        let mut most = 0;
        let lines = lines(&out)?;
        for line in &lines {
            running += if line == "start" { 1 } else { -1 };
            most = most.max(running);
        }
        assert_eq!(lines.len(), 10);
        assert_eq!(most, 2);
        Ok(())
    }

    #[tokio::test]
    async fn full_queue_drops_events() -> TestResult {
        let dir = tempfile::tempdir()?;
        let mut slow = hook(&[HookEvent::Started], "exec sleep 30", dir.path());
        slow.concurrency = 1;
        let hooks = Hooks::with_queue(vec![slow], 2);
        let sender = hooks.sender();
        // The worker does not run between these calls on the test's
        // single-threaded runtime, so only the queue holds events.
        let queued: usize = (0..10)
            .map(|_| sender.fire(HookEvent::Started, &BTreeMap::new()))
            .sum();
        assert_eq!(queued, 2);
        hooks.shutdown(Duration::ZERO).await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn full_queue_warnings_are_limited_per_hook() -> TestResult {
        let hooks = Hooks::with_queue(vec![unstartable(), unstartable()], 1);
        let sender = hooks.sender();
        let other = hooks.sender();
        sender.fire(HookEvent::Started, &BTreeMap::new());
        // Each hook drops two events and lets one warning through; the
        // check below is held back too.
        sender.fire(HookEvent::Started, &BTreeMap::new());
        other.fire(HookEvent::Started, &BTreeMap::new());
        for queue in sender.queues.iter() {
            assert_eq!(queue.full.check(), None, "one warning per hook");
        }
        tokio::time::advance(WARN_INTERVAL).await;
        let suppressed: Vec<Option<u64>> = sender.queues.iter().map(|q| q.full.check()).collect();
        assert_eq!(suppressed, [Some(2), Some(2)]);
        hooks.shutdown(Duration::ZERO).await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn failed_run_warnings_are_rate_limited() -> TestResult {
        let hooks = Hooks::spawn(vec![unstartable()]);
        let sender = hooks.sender();
        for _ in 0..5 {
            sender.fire(HookEvent::Started, &BTreeMap::new());
        }
        hooks.shutdown(Duration::from_secs(1)).await?;
        let failures = sender
            .queues
            .first()
            .map(|queue| Arc::clone(&queue.failures))
            .ok_or_else(|| TestError::Unexpected("no hook".to_owned()))?;
        // The first failure was warned about; the other four and this check are held back.
        assert_eq!(failures.check(), None);
        tokio::time::advance(WARN_INTERVAL).await;
        assert_eq!(failures.check(), Some(5));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_ends_commands_past_the_deadline() -> TestResult {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("pid");
        let mut long = hook(
            &[HookEvent::DaemonStopping],
            "echo $$ > \"$0\"; exec sleep 30",
            &out,
        );
        long.timeout_ms = 600_000;
        let hooks = Hooks::spawn(vec![long]);
        hooks
            .sender()
            .fire(HookEvent::DaemonStopping, &BTreeMap::new());
        let pid: u32 = wait_for_lines(&out, 1)
            .await?
            .first()
            .ok_or_else(|| TestError::Unexpected("no pid".to_owned()))?
            .parse()?;
        let started = Instant::now();
        hooks.shutdown(Duration::from_millis(100)).await?;
        assert!(started.elapsed() < Duration::from_secs(3));
        let gone = match std::fs::metadata(format!("/proc/{pid}")) {
            Ok(_) => false,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        assert!(gone, "the command was not reaped");
        Ok(())
    }
}
