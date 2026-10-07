//! Hook commands as Linux processes in their own process group.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Stdio;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use touchcue_core::StopSignal;

use crate::VAR_PREFIX;
use crate::runner::{Ending, Finished, KILL_GRACE, Stop};

/// Write end of a command's stdin pipe.
pub(crate) type Stdin = tokio::process::ChildStdin;

/// Longest wait for the output pipes to close after the command exited;
/// a background process of the command may hold them open.
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);
/// Bytes of stdout and of stderr kept from each run.
const OUTPUT_MAX: u64 = 4096;

/// Failure to run a hook command.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("the command is empty")]
    NoProgram,
    #[error("cannot start the command")]
    Spawn(#[source] io::Error),
    #[error("cannot wait for the command")]
    Wait(#[source] io::Error),
}

/// Runs `command` with `env` added to the inherited environment, without a
/// shell, and waits for it.
///
/// Stdin is `/dev/null`. When `timeout` passes or `kill` is cancelled, the
/// group gets `SIGTERM`, then `SIGKILL` once the command exited or after
/// [`KILL_GRACE`]. Otherwise as [`supervised`].
pub(crate) async fn run(
    command: &[String],
    env: &[(String, String)],
    timeout: Duration,
    kill: &CancellationToken,
) -> Result<Finished, RunError> {
    let stop = Stop {
        signal: StopSignal::Term,
        grace: KILL_GRACE,
        kill_group_on_exit: false,
    };
    supervised(command, env, false, stop, |_stdin| async move {
        tokio::select! {
            () = tokio::time::sleep(timeout) => Ending::TimedOut,
            () = kill.cancelled() => Ending::Killed,
        }
    })
    .await
}

/// Runs `command` with `env` added to the inherited environment, without a
/// shell, until it exits or `until` completes, and reaps it.
///
/// The command runs in a new process group; inherited variables starting
/// with [`VAR_PREFIX`] are removed. Stdin is a pipe whose write end is
/// passed to `until` when `piped` is set, else `/dev/null`. The first
/// [`OUTPUT_MAX`] bytes of stdout and stderr are kept and the rest is read
/// and discarded. When `until` completes first, the group gets
/// `stop.signal`, then `SIGKILL` once the command exited or after
/// `stop.grace`, and the run ends with the [`Ending`] `until` returned.
/// When the command exits first, the group gets `SIGKILL` if
/// `stop.kill_group_on_exit` is set. The command is always reaped before
/// this returns.
pub(crate) async fn supervised<F, U>(
    command: &[String],
    env: &[(String, String)],
    piped: bool,
    stop: Stop,
    until: F,
) -> Result<Finished, RunError>
where
    F: FnOnce(Option<Stdin>) -> U,
    U: Future<Output = Ending>,
{
    let (program, args) = command.split_first().ok_or(RunError::NoProgram)?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if piped { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    for key in reserved(std::env::vars_os().map(|(key, _)| key)) {
        cmd.env_remove(key);
    }
    cmd.envs(env.iter().map(|(key, value)| (key, value)));
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let group = group(&child);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut output = std::pin::pin!(async { tokio::join!(capture(stdout), capture(stderr)) });
    let mut captured = None;
    let ended = {
        let mut supervised = std::pin::pin!(supervise(&mut child, group, stop, until(stdin)));
        loop {
            tokio::select! {
                ended = &mut supervised => break ended,
                out = &mut output, if captured.is_none() => captured = Some(out),
            }
        }
    };
    let captured = match captured {
        Some(out) => Some(out),
        None => match tokio::time::timeout(OUTPUT_DRAIN, output).await {
            Ok(out) => Some(out),
            Err(_elapsed) => {
                tracing::debug!("output still open after the command exited");
                None
            }
        },
    };
    let (status, ending) = ended.map_err(RunError::Wait)?;
    let (stdout, stderr) = captured.unwrap_or_default();
    Ok(Finished {
        ending,
        code: status.code(),
        signal: status.signal(),
        stdout,
        stderr,
    })
}

/// Returns the variable names among `keys` that start with [`VAR_PREFIX`],
/// which a hook gets only from touchcue.
fn reserved(keys: impl Iterator<Item = OsString>) -> impl Iterator<Item = OsString> {
    keys.filter(|key| key.as_encoded_bytes().starts_with(VAR_PREFIX.as_bytes()))
}

/// Returns the process group of `child`, which leads it; `None` once it was reaped.
fn group(child: &Child) -> Option<Pid> {
    let id = child.id()?;
    match i32::try_from(id) {
        Ok(raw) => Pid::from_raw(raw),
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "child pid out of range"
            );
            None
        }
    }
}

/// Waits for `child`, ending its process group when `until` completes first.
async fn supervise(
    child: &mut Child,
    group: Option<Pid>,
    stop: Stop,
    until: impl Future<Output = Ending>,
) -> io::Result<(std::process::ExitStatus, Ending)> {
    let ending = tokio::select! {
        status = child.wait() => {
            let status = status?;
            if stop.kill_group_on_exit {
                // The group id stays reserved while a member lives; once all
                // are gone the id may be reused, a small window this accepts.
                signal(group, Signal::KILL);
            }
            return Ok((status, Ending::Exited));
        }
        ending = until => ending,
    };
    signal(group, stop_signal(stop.signal));
    let exited = match tokio::time::timeout(stop.grace, child.wait()).await {
        Ok(status) => Some(status?),
        Err(_elapsed) => None,
    };
    // Also ends processes of the group that outlived the command; the
    // group keeps its id while any of them exists.
    signal(group, Signal::KILL);
    let status = match exited {
        Some(status) => status,
        None => child.wait().await?,
    };
    Ok((status, ending))
}

fn stop_signal(signal: StopSignal) -> Signal {
    match signal {
        StopSignal::Term => Signal::TERM,
        StopSignal::Int => Signal::INT,
        StopSignal::Hup => Signal::HUP,
    }
}

/// Sends `signal` to `group`; a group that no longer exists is not an error.
fn signal(group: Option<Pid>, signal: Signal) {
    let Some(group) = group else {
        return;
    };
    if let Err(errno) = kill_process_group(group, signal) {
        tracing::debug!(
            error = &errno as &dyn std::error::Error,
            "cannot signal the process group"
        );
    }
}

/// Returns the first [`OUTPUT_MAX`] bytes of `pipe`, reading and
/// discarding the rest until it closes. A read error ends the capture.
async fn capture(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let read = async {
        (&mut pipe).take(OUTPUT_MAX).read_to_end(&mut kept).await?;
        tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await
    };
    if let Err(error) = read.await {
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            "cannot read hook output"
        );
    }
    kept
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] io::Error),
        #[error(transparent)]
        Run(#[from] RunError),
        #[error(transparent)]
        Int(#[from] std::num::ParseIntError),
        #[error(transparent)]
        Size(#[from] std::num::TryFromIntError),
        #[error("{0}")]
        Unexpected(String),
    }

    type TestResult = Result<(), TestError>;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()]
    }

    /// Returns whether process `pid` is gone or a zombie.
    fn ended(pid: u32) -> Result<bool, TestError> {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => Ok(stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    async fn wait_for_file(path: &Path) -> Result<String, TestError> {
        for _ in 0..200 {
            match std::fs::read_to_string(path) {
                Ok(text) if text.ends_with('\n') => return Ok(text),
                Ok(_partial) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(format!(
            "{} not written",
            path.display()
        )))
    }

    #[tokio::test]
    async fn exit_status_and_output_are_reported() -> TestResult {
        let finished = run(
            &sh("printf out; printf err >&2; exit 3"),
            &[],
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(finished.ending, Ending::Exited);
        assert_eq!(finished.code, Some(3));
        assert_eq!(finished.stdout, b"out");
        assert_eq!(finished.stderr, b"err");
        Ok(())
    }

    #[tokio::test]
    async fn output_is_capped_without_blocking_the_command() -> TestResult {
        let finished = run(
            &sh("head -c 1000000 /dev/zero"),
            &[],
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(finished.ending, Ending::Exited);
        assert_eq!(finished.code, Some(0));
        assert_eq!(u64::try_from(finished.stdout.len())?, OUTPUT_MAX);
        Ok(())
    }

    #[tokio::test]
    async fn arguments_reach_the_program_unchanged() -> TestResult {
        let finished = run(
            &[
                "printf".to_owned(),
                "%s|".to_owned(),
                "$HOME".to_owned(),
                "a b".to_owned(),
                ";true".to_owned(),
            ],
            &[],
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(finished.stdout, b"$HOME|a b|;true|");
        Ok(())
    }

    #[test]
    fn inherited_touchcue_variables_are_reserved() {
        let keys = [
            "TOUCHCUE_CONFIG",
            "PATH",
            "TOUCHCUE_LOG",
            "TOUCHCUEX",
            "touchcue_log",
        ]
        .map(OsString::from);
        let reserved: Vec<OsString> = reserved(keys.into_iter()).collect();
        assert_eq!(reserved, ["TOUCHCUE_CONFIG", "TOUCHCUE_LOG"]);
    }

    #[tokio::test]
    async fn missing_program_fails_to_spawn() {
        let result = run(
            &["/nonexistent/touchcue-hook".to_owned()],
            &[],
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(RunError::Spawn(_))), "{result:?}");
    }

    #[tokio::test]
    async fn timeout_ends_and_reaps_the_process_group() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pids = dir.path().join("pids");
        let script = format!(
            "trap '' TERM; sleep 30 & echo \"$$ $!\" > '{}'; wait",
            pids.display()
        );
        let started = Instant::now();
        let finished = run(
            &sh(&script),
            &[],
            Duration::from_millis(300),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(finished.ending, Ending::TimedOut);
        assert_eq!(
            finished.signal,
            Some(9),
            "SIGTERM is ignored, SIGKILL ends it"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let text = wait_for_file(&pids).await?;
        let (shell, sleeper) = text
            .trim()
            .split_once(' ')
            .ok_or_else(|| TestError::Unexpected(text.clone()))?;
        let (shell, sleeper): (u32, u32) = (shell.parse()?, sleeper.parse()?);
        assert!(ended(shell)?, "the command was not reaped");
        for _ in 0..200 {
            if ended(sleeper)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(TestError::Unexpected(
            "the background process survived".to_owned(),
        ))
    }

    #[tokio::test]
    async fn cancellation_kills_the_command() -> TestResult {
        let kill = CancellationToken::new();
        let task = {
            let kill = kill.clone();
            tokio::spawn(async move {
                run(&sh("exec sleep 30"), &[], Duration::from_secs(60), &kill).await
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        kill.cancel();
        let finished = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .map_err(|elapsed| TestError::Unexpected(elapsed.to_string()))?
            .map_err(|error| TestError::Unexpected(error.to_string()))??;
        assert_eq!(finished.ending, Ending::Killed);
        assert_eq!(finished.signal, Some(15));
        Ok(())
    }
}
