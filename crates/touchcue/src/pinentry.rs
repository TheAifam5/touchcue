//! touchcue as gpg-agent's `pinentry-program`.
//!
//! The wrapper starts the real pinentry, `$TOUCHCUE_PINENTRY` or else
//! `pinentry` on `PATH`, with the same arguments and environment plus
//! `TOUCHCUE_PINENTRY_ACTIVE=1`, copies both Assuan streams unchanged,
//! inherits stderr, forwards SIGTERM, SIGINT, SIGHUP, SIGUSR1 and SIGUSR2,
//! and exits with the child's status. It refuses to start when
//! `TOUCHCUE_PINENTRY_ACTIVE` is already set. It never answers a command
//! itself. It makes itself non-dumpable first and refuses to start when it
//! cannot.
//!
//! A copy of the stream is fed to an [`observer::Observer`]. When a `GETPIN`
//! or `CONFIRM` dialog starts after gpg-agent named the client it serves
//! with `OPTION owner`, the wrapper reports that client to the daemon over
//! its helper socket and reports the dialog's end with its reply. Reporting
//! is best effort: a missing daemon or a socket error never delays or alters
//! the Assuan stream.

pub mod observer;

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable holding the absolute path of the real pinentry.
pub const PROGRAM_ENV: &str = "TOUCHCUE_PINENTRY";
/// Program looked up on `PATH` when [`PROGRAM_ENV`] is unset or empty.
pub const DEFAULT_PROGRAM: &str = "pinentry";
/// Environment variable set for the real pinentry; the wrapper refuses to
/// start when it is present.
pub const ACTIVE_ENV: &str = "TOUCHCUE_PINENTRY_ACTIVE";

/// Failure to run the wrapper.
#[derive(Debug, thiserror::Error)]
pub enum PinentryError {
    #[error("{PROGRAM_ENV} must be an absolute path")]
    NotAbsolute { path: PathBuf },
    #[error("no {DEFAULT_PROGRAM} found on PATH")]
    NotFound,
    #[error("the real pinentry is missing")]
    Missing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot locate the touchcue executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("the real pinentry is touchcue itself; refusing to start it")]
    Recursive { path: PathBuf },
    #[error("{ACTIVE_ENV} is set: touchcue pinentry would start itself; refusing to start")]
    Nested,
    #[error("the real pinentry is not an executable file")]
    NotExecutable { path: PathBuf },
    #[cfg(target_os = "linux")]
    #[error("cannot make the wrapper non-dumpable; refusing to start")]
    Dumpable(#[source] rustix::io::Errno),
    #[error("cannot {action}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("the pinentry wrapper is not supported on this platform")]
    Unsupported,
}

/// Runs the wrapper with `args`, the arguments after the program name, and
/// returns the exit code to use: the child's code, or 128 plus the signal
/// that ended it.
///
/// Nothing is written to stdout, which belongs to the Assuan stream.
///
/// # Errors
///
/// Returns [`PinentryError`] when the process cannot be made
/// non-dumpable, the real pinentry cannot be located or
/// started, or stdio cannot be taken over. Nothing has been written to
/// stdout then.
#[cfg(target_os = "linux")]
pub fn run(args: &[OsString]) -> Result<std::process::ExitCode, PinentryError> {
    run_with(args, crate::dumpable::forbid)
}

/// [`run`] with the function that makes the process non-dumpable.
#[cfg(target_os = "linux")]
fn run_with(
    args: &[OsString],
    forbid_dumps: fn() -> Result<(), rustix::io::Errno>,
) -> Result<std::process::ExitCode, PinentryError> {
    forbid_dumps().map_err(PinentryError::Dumpable)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| PinentryError::Io {
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
/// Always returns [`PinentryError::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn run(_args: &[OsString]) -> Result<std::process::ExitCode, PinentryError> {
    Err(PinentryError::Unsupported)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File};
    use std::io::{self, ErrorKind};
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::os::unix::process::ExitStatusExt as _;
    use std::path::{Path, PathBuf};
    use std::pin::pin;
    use std::process::{ExitCode, ExitStatus, Stdio};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use rustix::process::{Pid, Signal, kill_process};
    use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};
    use tokio::signal::unix::{SignalKind, signal};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use touchcue_core::helper::{Message, Origin};

    use super::observer::{Event, Observer};
    use super::{ACTIVE_ENV, DEFAULT_PROGRAM, PROGRAM_ENV, PinentryError};
    use crate::scdaemon::session::Reporter;

    /// Longest wait for runtime tasks after the wrapper finished.
    pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
    /// Longest wait to deliver queued reports after pinentry exited.
    const REPORT_DRAIN: Duration = Duration::from_millis(500);
    /// Longest wait for pinentry's remaining output after it exited.
    const OUTPUT_DRAIN: Duration = Duration::from_secs(1);
    /// Read buffer of each direction.
    const BUFFER: usize = 8 * 1024;

    /// Which way bytes flow.
    #[derive(Debug, Clone, Copy)]
    enum Side {
        /// gpg-agent to pinentry.
        Client,
        /// pinentry to gpg-agent.
        Server,
    }

    #[tracing::instrument(name = "pinentry", skip_all)]
    pub(super) async fn wrap(args: &[OsString]) -> Result<ExitCode, PinentryError> {
        if std::env::var_os(ACTIVE_ENV).is_some() {
            return Err(PinentryError::Nested);
        }
        let me = fs::metadata("/proc/self/exe")
            .map(|meta| (meta.dev(), meta.ino()))
            .map_err(PinentryError::CurrentExe)?;
        let program = real_pinentry(
            std::env::var_os(PROGRAM_ENV).as_deref(),
            std::env::var_os("PATH").as_deref(),
            me,
        )?;
        let mut signals = Forwarded::register()?;
        let (agent_in, agent_out) = take_stdio()?;
        let mut child = Command::new(&program)
            .args(args)
            .env(ACTIVE_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(io_error("start the real pinentry"))?;
        let (Some(child_in), Some(child_out)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(PinentryError::Io {
                action: "connect to the real pinentry",
                source: io::Error::from(ErrorKind::BrokenPipe),
            });
        };

        let (reporter, delivery) = Reporter::spawn();
        let session = Session::new(reporter);
        let status = {
            let mut to_child = pin!(pump(agent_in, child_in, Side::Client, &session));
            let mut to_agent = pin!(pump(child_out, agent_out, Side::Server, &session));
            let (mut to_child_done, mut to_agent_done) = (false, false);
            let status = loop {
                tokio::select! {
                    status = child.wait() => break status,
                    () = &mut to_child, if !to_child_done => to_child_done = true,
                    () = &mut to_agent, if !to_agent_done => to_agent_done = true,
                    Some(()) = signals.terminate.recv() => forward(&child, Signal::TERM),
                    Some(()) = signals.interrupt.recv() => forward(&child, Signal::INT),
                    Some(()) = signals.hangup.recv() => forward(&child, Signal::HUP),
                    Some(()) = signals.user1.recv() => forward(&child, Signal::USR1),
                    Some(()) = signals.user2.recv() => forward(&child, Signal::USR2),
                }
            };
            if !to_agent_done {
                tokio::select! {
                    drained = timeout(OUTPUT_DRAIN, &mut to_agent) => {
                        if let Err(error) = drained {
                            tracing::debug!(
                                error = &error as &dyn std::error::Error,
                                "pinentry output not drained in time"
                            );
                        }
                    }
                    () = signals.stopped() => {
                        tracing::debug!("pinentry output drain stopped by a signal");
                    }
                }
            }
            session.close();
            status
        };
        drop(session);
        drain_reports(delivery, &mut signals).await;
        let status = status.map_err(io_error("wait for the real pinentry"))?;
        Ok(exit_code(status))
    }

    /// Waits up to [`REPORT_DRAIN`] for `delivery`, aborting it then or on
    /// SIGTERM or SIGINT.
    async fn drain_reports(mut delivery: JoinHandle<()>, signals: &mut Forwarded) {
        let delivered = tokio::select! {
            delivered = timeout(REPORT_DRAIN, &mut delivery) => Some(delivered),
            () = signals.stopped() => None,
        };
        let abort = match delivered {
            Some(Ok(Ok(()))) => false,
            Some(Ok(Err(error))) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "report delivery task failed"
                );
                false
            }
            Some(Err(error)) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "reports not delivered in time"
                );
                true
            }
            None => {
                tracing::debug!("report delivery stopped by a signal");
                true
            }
        };
        if abort {
            delivery.abort();
            match delivery.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "report delivery task failed"
                ),
            }
        }
    }

    /// Returns the configured absolute path or the first executable
    /// [`DEFAULT_PROGRAM`] on `path`, excluding the running touchcue.
    #[tracing::instrument(skip_all, err(level = "debug"))]
    pub(super) fn real_pinentry(
        configured: Option<&OsStr>,
        path: Option<&OsStr>,
        me: (u64, u64),
    ) -> Result<PathBuf, PinentryError> {
        if let Some(program) = configured.filter(|program| !program.is_empty()) {
            let program = Path::new(program);
            if !program.is_absolute() {
                return Err(PinentryError::NotAbsolute {
                    path: program.to_owned(),
                });
            }
            return usable(program, me);
        }
        let found = path
            .into_iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(DEFAULT_PROGRAM))
            .find(|candidate| match fs::metadata(candidate) {
                Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
                Err(error) => {
                    tracing::trace!(
                        error = &error as &dyn std::error::Error,
                        "pinentry candidate skipped"
                    );
                    false
                }
            });
        match found {
            Some(program) => usable(&program, me),
            None => Err(PinentryError::NotFound),
        }
    }

    /// Returns `program` when it is an executable regular file that is not
    /// the running touchcue, whose device and inode are `me`.
    #[tracing::instrument(skip_all, err(level = "debug"))]
    fn usable(program: &Path, me: (u64, u64)) -> Result<PathBuf, PinentryError> {
        let meta = fs::metadata(program).map_err(|source| PinentryError::Missing {
            path: program.to_owned(),
            source,
        })?;
        if (meta.dev(), meta.ino()) == me {
            return Err(PinentryError::Recursive {
                path: program.to_owned(),
            });
        }
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return Err(PinentryError::NotExecutable {
                path: program.to_owned(),
            });
        }
        Ok(program.to_owned())
    }

    /// Observer of one pinentry session and the dialog it reported.
    struct Session {
        state: Mutex<State>,
        reporter: Reporter,
    }

    struct State {
        observer: Observer,
        /// Sequence number of the reported dialog that has not ended.
        shown: Option<u32>,
    }

    impl Session {
        fn new(reporter: Reporter) -> Self {
            Self {
                state: Mutex::new(State {
                    observer: Observer::new(),
                    shown: None,
                }),
                reporter,
            }
        }

        fn lock(&self) -> MutexGuard<'_, State> {
            match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    tracing::warn!(error = %poisoned, "observer lock poisoned; continuing");
                    poisoned.into_inner()
                }
            }
        }

        #[tracing::instrument(skip_all)]
        fn observe(&self, side: Side, bytes: &[u8]) {
            let mut events = Vec::new();
            let mut state = self.lock();
            let disabled = state.observer.disabled();
            match side {
                Side::Client => state.observer.client(bytes, &mut events),
                Side::Server => state.observer.server(bytes, &mut events),
            }
            if !disabled && state.observer.disabled() {
                tracing::warn!("overlong Assuan line; pinentry dialogs no longer reported");
            }
            self.apply(&mut state, events);
        }

        /// Ends observing, reporting an unanswered dialog as failed.
        #[tracing::instrument(skip_all)]
        fn close(&self) {
            let mut events = Vec::new();
            let mut state = self.lock();
            state.observer.close(&mut events);
            self.apply(&mut state, events);
        }

        fn apply(&self, state: &mut State, events: Vec<Event>) {
            for event in events {
                match event {
                    Event::Open { pid, uid } => {
                        let seq = self.reporter.next_seq();
                        state.shown = Some(seq);
                        tracing::debug!(seq, pid, "pinentry dialog opened");
                        self.reporter.send(Message::Owner { seq, pid, uid });
                    }
                    Event::Close(outcome) => {
                        if let Some(seq) = state.shown.take() {
                            tracing::debug!(seq, ?outcome, "pinentry dialog ended");
                            self.reporter.send(Message::End {
                                origin: Origin::Pinentry,
                                seq,
                                outcome,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Copies `from` to `to` until EOF or an error, feeding every chunk to
    /// `session` first, then shuts `to` down.
    #[tracing::instrument(skip_all, fields(?side))]
    async fn pump<R, W>(mut from: R, mut to: W, side: Side, session: &Session)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buffer = [0; BUFFER];
        loop {
            let read = match from.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::debug!(
                        error = &error as &dyn std::error::Error,
                        ?side,
                        "read failed"
                    );
                    break;
                }
            };
            let Some(chunk) = buffer.get(..read) else {
                break;
            };
            session.observe(side, chunk);
            if let Err(error) = to.write_all(chunk).await {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    ?side,
                    "write failed"
                );
                break;
            }
            if let Err(error) = to.flush().await {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    ?side,
                    "flush failed"
                );
                break;
            }
        }
        session.close();
        if let Err(error) = to.shutdown().await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                ?side,
                "close failed"
            );
        }
    }

    type Reader = Box<dyn AsyncRead + Unpin>;
    type Writer = Box<dyn AsyncWrite + Unpin>;

    fn io_error(action: &'static str) -> impl FnOnce(io::Error) -> PinentryError {
        move |source| PinentryError::Io { action, source }
    }

    /// Signals passed on to pinentry instead of acting on the wrapper.
    struct Forwarded {
        terminate: tokio::signal::unix::Signal,
        interrupt: tokio::signal::unix::Signal,
        hangup: tokio::signal::unix::Signal,
        user1: tokio::signal::unix::Signal,
        user2: tokio::signal::unix::Signal,
    }

    impl Forwarded {
        #[tracing::instrument(skip_all, err(level = "debug"))]
        fn register() -> Result<Self, PinentryError> {
            let register = |kind, action| signal(kind).map_err(io_error(action));
            Ok(Self {
                terminate: register(SignalKind::terminate(), "handle SIGTERM")?,
                interrupt: register(SignalKind::interrupt(), "handle SIGINT")?,
                hangup: register(SignalKind::hangup(), "handle SIGHUP")?,
                user1: register(SignalKind::user_defined1(), "handle SIGUSR1")?,
                user2: register(SignalKind::user_defined2(), "handle SIGUSR2")?,
            })
        }

        /// Waits for SIGTERM or SIGINT.
        async fn stopped(&mut self) {
            tokio::select! {
                Some(()) = self.terminate.recv() => {}
                Some(()) = self.interrupt.recv() => {}
                else => std::future::pending().await,
            }
        }
    }

    /// Replaces stdin and stdout with `/dev/null`, returning their previous
    /// handles as the sole references to the Assuan streams.
    #[tracing::instrument(skip_all, err(level = "debug"))]
    fn take_stdio() -> Result<(Reader, Writer), PinentryError> {
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
        rustix::stdio::dup2_stdin(&null).map_err(|errno| PinentryError::Io {
            action: "replace stdin",
            source: errno.into(),
        })?;
        rustix::stdio::dup2_stdout(&null).map_err(|errno| PinentryError::Io {
            action: "replace stdout",
            source: errno.into(),
        })?;
        Ok((reader(input)?, writer(output)?))
    }

    /// Wraps a pipe for async reads; any other file is read on the blocking
    /// pool.
    #[tracing::instrument(skip_all, err(level = "debug"))]
    fn reader(fd: OwnedFd) -> Result<Reader, PinentryError> {
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
    #[tracing::instrument(skip_all, err(level = "debug"))]
    fn writer(fd: OwnedFd) -> Result<Writer, PinentryError> {
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
    #[tracing::instrument(skip_all)]
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

        fn identity(path: &Path) -> Result<(u64, u64), TestError> {
            let meta = fs::metadata(path)?;
            Ok((meta.dev(), meta.ino()))
        }

        #[test]
        fn configured_program_must_be_absolute_and_not_touchcue() -> Result<(), TestError> {
            let dir = tempfile::tempdir()?;
            let exe = dir.path().join("touchcue");
            file(&exe, 0o755)?;
            let me = identity(&exe)?;
            let program = dir.path().join("pinentry-gtk");
            file(&program, 0o755)?;
            let link = dir.path().join("touchcue-pinentry");
            symlink(&exe, &link)?;

            assert!(matches!(
                real_pinentry(Some(program.as_os_str()), None, me),
                Ok(path) if path == program
            ));
            assert!(matches!(
                real_pinentry(Some(OsStr::new("pinentry-gtk")), None, me),
                Err(PinentryError::NotAbsolute { .. })
            ));
            assert!(matches!(
                real_pinentry(Some(link.as_os_str()), None, me),
                Err(PinentryError::Recursive { .. })
            ));
            assert!(matches!(
                real_pinentry(Some(dir.path().join("missing").as_os_str()), None, me),
                Err(PinentryError::Missing { .. })
            ));
            assert!(matches!(
                real_pinentry(Some(dir.path().as_os_str()), None, me),
                Err(PinentryError::NotExecutable { .. })
            ));
            Ok(())
        }

        #[test]
        fn path_lookup_takes_the_first_executable() -> Result<(), TestError> {
            let root = tempfile::tempdir()?;
            let exe = root.path().join("touchcue");
            file(&exe, 0o755)?;
            let me = identity(&exe)?;
            let (plain, real, own) = (
                root.path().join("plain"),
                root.path().join("real"),
                root.path().join("own"),
            );
            for dir in [&plain, &real, &own] {
                fs::create_dir(dir)?;
            }
            file(&plain.join(DEFAULT_PROGRAM), 0o644)?;
            file(&real.join(DEFAULT_PROGRAM), 0o755)?;
            symlink(&exe, own.join(DEFAULT_PROGRAM))?;
            let search = |dirs: &[&Path]| {
                let mut path = OsString::new();
                for dir in dirs {
                    if !path.is_empty() {
                        path.push(":");
                    }
                    path.push(dir);
                }
                real_pinentry(Some(OsStr::new("")), Some(&path), me)
            };

            assert!(matches!(
                search(&[Path::new("relative"), &plain, &real, &own]),
                Ok(path) if path == real.join(DEFAULT_PROGRAM)
            ));
            assert!(matches!(
                search(&[&own, &real]),
                Err(PinentryError::Recursive { .. })
            ));
            assert!(matches!(search(&[&plain]), Err(PinentryError::NotFound)));
            assert!(matches!(
                real_pinentry(None, None, me),
                Err(PinentryError::NotFound)
            ));
            Ok(())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_start_when_dumps_cannot_be_forbidden() {
        let result = run_with(&[], || Err(rustix::io::Errno::PERM));
        assert!(
            matches!(
                result,
                Err(PinentryError::Dumpable(rustix::io::Errno::PERM))
            ),
            "{result:?}"
        );
    }
}
