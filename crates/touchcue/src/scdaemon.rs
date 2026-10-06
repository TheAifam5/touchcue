//! touchcue as gpg-agent's `scdaemon-program`.
//!
//! gpg-agent starts the program with the arguments `--multi-server`, plus
//! `--homedir <dir>` for a non-default home, and speaks Assuan over its stdin
//! and stdout. The wrapper starts the real scdaemon from
//! `gpgconf --list-dirs libexecdir` with the same arguments and environment,
//! copies both streams unchanged, inherits stderr, forwards SIGTERM, SIGINT,
//! SIGHUP, SIGUSR1 and SIGUSR2, and exits with the child's status. The
//! wrapper makes itself non-dumpable, which keeps other processes of the
//! user from reading its own memory; PINs and data also pass through
//! gpg-agent and the real scdaemon, which this does not cover.
//!
//! gpg-agent also connects to scdaemon's own socket when the pipe is in use.
//! The wrapper proxies that socket as well; when the proxy cannot be set up,
//! only the pipe is tracked.
//!
//! Every connection is fed to a [`tracker::Tracker`], and card operations
//! that stay silent for [`SHOW_DELAY`] are reported to the daemon over its
//! helper socket. Reporting is best effort: a missing daemon or a socket
//! error never delays or alters the Assuan stream.

#[cfg(target_os = "linux")]
mod proxy;
#[cfg(target_os = "linux")]
mod session;
pub mod tracker;

use std::ffi::OsString;
use std::time::Duration;

/// First argument gpg-agent passes to its `scdaemon-program`.
pub const MULTI_SERVER: &str = "--multi-server";

/// How long a card operation stays silent before it is reported as waiting
/// for a touch.
pub const SHOW_DELAY: Duration = Duration::from_millis(400);

/// Whether the arguments after the program name are gpg-agent's scdaemon
/// invocation.
#[must_use]
pub fn is_invocation(args: &[OsString]) -> bool {
    args.first().is_some_and(|arg| arg == MULTI_SERVER)
}

/// Failure to run the wrapper.
#[derive(Debug, thiserror::Error)]
pub enum ScdaemonError {
    #[error("cannot locate the real scdaemon")]
    Locate(#[source] crate::gpg::GpgError),
    #[error("the real scdaemon {} is missing", path.display())]
    Missing {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot locate the touchcue executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("{} is touchcue itself; refusing to start it", path.display())]
    Recursive { path: std::path::PathBuf },
    #[error("{} is not an executable file", path.display())]
    NotExecutable { path: std::path::PathBuf },
    #[error("cannot {action}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("the scdaemon wrapper is not supported on this platform")]
    Unsupported,
}

/// Runs the wrapper with `args`, the arguments after the program name, and
/// returns the exit code to use: the child's code, or 128 plus the signal
/// that ended it.
///
/// Diagnostics go only through `tracing`; nothing is written to stdout,
/// which belongs to the Assuan stream, or directly to stderr, which is
/// scdaemon's log channel.
///
/// # Errors
///
/// Returns [`ScdaemonError`] when the real scdaemon cannot be located or
/// started, or stdio cannot be taken over. Nothing has been written to
/// stdout then.
#[cfg(target_os = "linux")]
pub fn run(args: &[OsString]) -> Result<std::process::ExitCode, ScdaemonError> {
    // Reset on exec, so the real scdaemon keeps its own setting.
    if let Err(error) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "cannot make the wrapper non-dumpable"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| ScdaemonError::Io {
            action: "start the async runtime",
            source,
        })?;
    let result = runtime.block_on(linux::wrap(args));
    runtime.shutdown_timeout(linux::SHUTDOWN_TIMEOUT);
    result
}

/// Runs the wrapper; unsupported outside Linux.
///
/// # Errors
///
/// Always returns [`ScdaemonError::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn run(_args: &[OsString]) -> Result<std::process::ExitCode, ScdaemonError> {
    Err(ScdaemonError::Unsupported)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::{self, ErrorKind};
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::os::unix::process::ExitStatusExt as _;
    use std::path::{Path, PathBuf};
    use std::pin::pin;
    use std::process::{ExitCode, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use rustix::process::{Pid, Signal, kill_process};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};
    use tokio::signal::unix::{SignalKind, signal};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::ScdaemonError;
    use super::proxy;
    use super::session::{Reporter, Session, Side, pump};

    /// The running executable, even when its file was replaced or removed.
    const CURRENT_EXE: &str = "/proc/self/exe";
    /// Real scdaemon locations tried in order when gpgconf fails.
    const FALLBACK_PROGRAMS: [&str; 3] = [
        "/usr/libexec/scdaemon",
        "/usr/lib/gnupg/scdaemon",
        "/usr/lib/gnupg2/scdaemon",
    ];
    /// Longest wait for runtime tasks after the wrapper finished.
    pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
    /// Longest wait to deliver queued reports after scdaemon exited.
    const REPORT_DRAIN: Duration = Duration::from_millis(500);
    /// Longest wait for scdaemon's remaining output after it exited.
    const OUTPUT_DRAIN: Duration = Duration::from_secs(1);
    /// Longest wait for the socket proxy to close its connections and clean
    /// up after scdaemon exited.
    const PROXY_DRAIN: Duration = Duration::from_secs(1);

    type Reader = Box<dyn AsyncRead + Unpin>;
    type Writer = Box<dyn AsyncWrite + Unpin>;

    fn io_error(action: &'static str) -> impl FnOnce(io::Error) -> ScdaemonError {
        move |source| ScdaemonError::Io { action, source }
    }

    #[tracing::instrument(name = "scdaemon", skip_all)]
    pub(super) async fn wrap(args: &[OsString]) -> Result<ExitCode, ScdaemonError> {
        let program = real_scdaemon().await?;
        let mut signals = Forwarded::register()?;
        let (agent_in, agent_out) = take_stdio()?;
        let mut child = Command::new(&program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(io_error("start the real scdaemon"))?;
        let (Some(child_in), Some(child_out)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ScdaemonError::Io {
                action: "connect to the real scdaemon",
                source: io::Error::from(ErrorKind::BrokenPipe),
            });
        };

        let (reporter, delivery) = Reporter::spawn();
        let session = Session::new(reporter.clone());
        let exited = AtomicBool::new(false);
        let stop = CancellationToken::new();
        let status = {
            let mut to_child = pin!(pump(agent_in, Box::new(child_in), Side::Client, &session));
            let mut to_agent = pin!(pump(Box::new(child_out), agent_out, Side::Server, &session));
            let mut timer = pin!(session.timer());
            let mut proxy = pin!(proxy::run(
                args.to_vec(),
                child.id(),
                &session,
                reporter,
                stop.clone(),
                || !exited.load(Ordering::Relaxed),
            ));
            let (mut to_child_done, mut to_agent_done, mut proxy_done) = (false, false, false);
            let status = loop {
                tokio::select! {
                    status = child.wait() => break status,
                    () = &mut to_child, if !to_child_done => to_child_done = true,
                    () = &mut to_agent, if !to_agent_done => to_agent_done = true,
                    result = &mut proxy, if !proxy_done => {
                        proxy_done = true;
                        proxy_finished(result);
                    }
                    Some(()) = signals.terminate.recv() => forward(&child, Signal::TERM),
                    Some(()) = signals.interrupt.recv() => forward(&child, Signal::INT),
                    Some(()) = signals.hangup.recv() => forward(&child, Signal::HUP),
                    Some(()) = signals.user1.recv() => forward(&child, Signal::USR1),
                    Some(()) = signals.user2.recv() => forward(&child, Signal::USR2),
                    () = &mut timer => {}
                }
            };
            exited.store(true, Ordering::Relaxed);
            stop.cancel();
            if !to_agent_done {
                drain(
                    "scdaemon output",
                    timeout(OUTPUT_DRAIN, &mut to_agent).await,
                );
            }
            if !proxy_done {
                match timeout(PROXY_DRAIN, &mut proxy).await {
                    Ok(result) => proxy_finished(result),
                    Err(error) => drain("socket proxy", Err(error)),
                }
            }
            session.close();
            status
        };
        // Drops the last reporter, so delivery ends after the queued reports.
        drop(session);
        match timeout(REPORT_DRAIN, delivery).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!(
                error = &error as &dyn std::error::Error,
                "report delivery task failed"
            ),
            Err(error) => drain("reports", Err(error)),
        }
        let status = status.map_err(io_error("wait for the real scdaemon"))?;
        Ok(exit_code(status))
    }

    /// Signals passed on to scdaemon instead of acting on the wrapper.
    struct Forwarded {
        terminate: tokio::signal::unix::Signal,
        interrupt: tokio::signal::unix::Signal,
        hangup: tokio::signal::unix::Signal,
        user1: tokio::signal::unix::Signal,
        user2: tokio::signal::unix::Signal,
    }

    impl Forwarded {
        fn register() -> Result<Self, ScdaemonError> {
            let register = |kind, action| signal(kind).map_err(io_error(action));
            Ok(Self {
                terminate: register(SignalKind::terminate(), "handle SIGTERM")?,
                interrupt: register(SignalKind::interrupt(), "handle SIGINT")?,
                hangup: register(SignalKind::hangup(), "handle SIGHUP")?,
                user1: register(SignalKind::user_defined1(), "handle SIGUSR1")?,
                user2: register(SignalKind::user_defined2(), "handle SIGUSR2")?,
            })
        }
    }

    fn drain(what: &'static str, result: Result<(), tokio::time::error::Elapsed>) {
        if let Err(error) = result {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                what,
                "not finished in time"
            );
        }
    }

    fn proxy_finished(result: Result<(), proxy::ProxyError>) {
        if let Err(error) = result {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "scdaemon socket not proxied"
            );
        }
    }

    /// Returns the real scdaemon: the one in `gpgconf --list-dirs
    /// libexecdir`, or when gpgconf cannot run or fails, the first usable of
    /// [`FALLBACK_PROGRAMS`]. A program that resolves to touchcue itself is
    /// never used.
    async fn real_scdaemon() -> Result<PathBuf, ScdaemonError> {
        let me = fs::metadata(CURRENT_EXE)
            .map(|meta| (meta.dev(), meta.ino()))
            .map_err(ScdaemonError::CurrentExe)?;
        let error = match crate::gpg::gpgconf_dir(None, "libexecdir").await {
            Ok(dir) => return usable(&dir.join("scdaemon"), me),
            Err(error) => ScdaemonError::Locate(error),
        };
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            "trying the known scdaemon paths"
        );
        for candidate in FALLBACK_PROGRAMS {
            match usable(Path::new(candidate), me) {
                Ok(program) => return Ok(program),
                Err(skipped) => tracing::debug!(
                    error = &skipped as &dyn std::error::Error,
                    "scdaemon candidate skipped"
                ),
            }
        }
        Err(error)
    }

    /// Returns `program` when it is an executable regular file that is not
    /// the running touchcue, whose device and inode are `me`. A hard link to
    /// touchcue is the same inode; a copy is caught when it runs, since its
    /// own check then finds itself.
    fn usable(program: &Path, me: (u64, u64)) -> Result<PathBuf, ScdaemonError> {
        let meta = fs::metadata(program).map_err(|source| ScdaemonError::Missing {
            path: program.to_owned(),
            source,
        })?;
        if (meta.dev(), meta.ino()) == me {
            return Err(ScdaemonError::Recursive {
                path: program.to_owned(),
            });
        }
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(ScdaemonError::NotExecutable {
                path: program.to_owned(),
            });
        }
        Ok(program.to_owned())
    }

    /// Takes gpg-agent's pipes off stdin and stdout and puts `/dev/null` in
    /// their place, so the returned handles are the only references: closing
    /// one signals EOF, and no stray write can reach the Assuan stream.
    fn take_stdio() -> Result<(Reader, Writer), ScdaemonError> {
        let input = io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(io_error("duplicate stdin"))?;
        let output = io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map_err(io_error("duplicate stdout"))?;
        let null = File::options()
            .read(true)
            .write(true)
            .open("/dev/null")
            .map_err(io_error("open /dev/null"))?;
        rustix::stdio::dup2_stdin(&null).map_err(|errno| ScdaemonError::Io {
            action: "replace stdin",
            source: errno.into(),
        })?;
        rustix::stdio::dup2_stdout(&null).map_err(|errno| ScdaemonError::Io {
            action: "replace stdout",
            source: errno.into(),
        })?;
        Ok((reader(input)?, writer(output)?))
    }

    /// Wraps a pipe for async reads; any other file is read on the blocking
    /// pool.
    fn reader(fd: OwnedFd) -> Result<Reader, ScdaemonError> {
        let copy = fd.try_clone().map_err(io_error("duplicate stdin"))?;
        match pipe::Receiver::from_owned_fd(copy) {
            Ok(receiver) => Ok(Box::new(receiver)),
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "stdin is not a pipe; reading it on the blocking pool"
                );
                Ok(Box::new(tokio::fs::File::from_std(File::from(fd))))
            }
        }
    }

    /// Wraps a pipe for async writes; any other file is written on the
    /// blocking pool.
    fn writer(fd: OwnedFd) -> Result<Writer, ScdaemonError> {
        let copy = fd.try_clone().map_err(io_error("duplicate stdout"))?;
        match pipe::Sender::from_owned_fd(copy) {
            Ok(sender) => Ok(Box::new(sender)),
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "stdout is not a pipe; writing it on the blocking pool"
                );
                Ok(Box::new(tokio::fs::File::from_std(File::from(fd))))
            }
        }
    }

    /// Sends `signal` to the child; it has not been reaped yet.
    fn forward(child: &Child, signal: Signal) {
        let Some(id) = child.id() else {
            return;
        };
        let pid = match i32::try_from(id) {
            Ok(raw) => Pid::from_raw(raw),
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "child pid out of range"
                );
                return;
            }
        };
        let Some(pid) = pid else {
            return;
        };
        if let Err(errno) = kill_process(pid, signal) {
            tracing::debug!(
                error = &errno as &dyn std::error::Error,
                "cannot forward signal"
            );
        }
    }

    /// Returns the child's exit code, or 128 plus the signal that ended it.
    fn exit_code(status: ExitStatus) -> ExitCode {
        let Some(code) = status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
        else {
            return ExitCode::FAILURE;
        };
        match u8::try_from(code) {
            Ok(code) => ExitCode::from(code),
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "exit code out of range"
                );
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs::Permissions;
        use std::os::unix::fs::symlink;

        use super::*;

        #[derive(Debug, thiserror::Error)]
        enum TestError {
            #[error(transparent)]
            Io(#[from] io::Error),
        }

        fn file(path: &Path, mode: u32) -> Result<(), TestError> {
            fs::write(path, "")?;
            fs::set_permissions(path, Permissions::from_mode(mode))?;
            Ok(())
        }

        #[test]
        fn usable_needs_an_executable_file_that_is_not_touchcue() -> Result<(), TestError> {
            let dir = tempfile::tempdir()?;
            let exe = dir.path().join("touchcue");
            file(&exe, 0o755)?;
            let meta = fs::metadata(&exe)?;
            let me = (meta.dev(), meta.ino());
            let hard = dir.path().join("hard");
            fs::hard_link(&exe, &hard)?;
            let program = dir.path().join("scdaemon");
            file(&program, 0o755)?;
            let plain = dir.path().join("plain");
            file(&plain, 0o644)?;
            let link = dir.path().join("link");
            symlink(&exe, &link)?;

            assert!(matches!(usable(&program, me), Ok(path) if path == program));
            assert!(matches!(
                usable(&plain, me),
                Err(ScdaemonError::NotExecutable { .. })
            ));
            assert!(matches!(
                usable(dir.path(), me),
                Err(ScdaemonError::NotExecutable { .. })
            ));
            assert!(matches!(
                usable(&dir.path().join("missing"), me),
                Err(ScdaemonError::Missing { .. })
            ));
            for itself in [&exe, &link, &hard] {
                assert!(matches!(
                    usable(itself, me),
                    Err(ScdaemonError::Recursive { .. })
                ));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_multi_server_first_is_an_invocation() {
        let args = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_invocation(&args(&["--multi-server"])));
        assert!(is_invocation(&args(&["--multi-server", "--homedir", "/g"])));
        assert!(!is_invocation(&args(&["run"])));
        assert!(!is_invocation(&args(&["run", "--multi-server"])));
        assert!(!is_invocation(&args(&[])));
    }
}
