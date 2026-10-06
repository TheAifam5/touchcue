//! `touchcue askpass`: an OpenSSH `SSH_ASKPASS` program that reports
//! security-key user-presence requests to the daemon.
//!
//! OpenSSH runs the askpass program with the message as its only argument.
//! For a user-presence notice it sets `SSH_ASKPASS_PROMPT=none`, connects
//! stdin and stdout to `/dev/null`, ignores the exit status, and sends
//! SIGTERM once the presence check finishes. That signal does not say
//! whether the key was touched or the check failed, so the end is always
//! reported as [`Outcome::Touched`](touchcue_core::Outcome::Touched).
//!
//! Every other prompt, a passphrase (`SSH_ASKPASS_PROMPT` unset) or a
//! confirmation (`confirm`), is handed to the program named by
//! [`FALLBACK_ENV`], which replaces this process, so touchcue never reads a
//! passphrase.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use touchcue_core::text::sanitize;

/// Environment variable naming the askpass program that answers every
/// prompt other than a user-presence notice.
///
/// It is removed from the fallback's environment, so a fallback that points
/// back to touchcue fails instead of looping.
pub const FALLBACK_ENV: &str = "TOUCHCUE_ASKPASS_FALLBACK";

/// Environment variable in which OpenSSH names the prompt type.
const PROMPT_ENV: &str = "SSH_ASKPASS_PROMPT";
/// `SSH_ASKPASS_PROMPT` value of a user-presence notice.
const NOTIFY_PROMPT: &str = "none";

const PRESENCE_PREFIX: &str = "Confirm user presence for key ";
const DESTINATION_PREFIX: &str = "public key authentication request for user \"";
const DESTINATION_SUFFIX: &str = "\" to listed host";

/// Longest detail sent to the daemon, in characters.
const MAX_DETAIL_CHARS: usize = 200;

/// Key and destination named in a user-presence notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInfo {
    /// Key type as OpenSSH prints it, such as `ED25519-SK`.
    pub key_type: String,
    /// Key fingerprint, such as `SHA256:…`.
    pub fingerprint: String,
    /// Remote user of an ssh-agent signing request; untrusted.
    pub user: Option<String>,
}

impl PromptInfo {
    /// Returns `<key type> <fingerprint>[ → user <user>]`, sanitized and
    /// capped at 200 characters.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        let mut detail = format!("{} {}", self.key_type, self.fingerprint);
        if let Some(user) = &self.user {
            detail.push_str(" → user ");
            detail.push_str(user);
        }
        sanitize(&detail, MAX_DETAIL_CHARS)
    }
}

/// Parses an OpenSSH user-presence notice.
///
/// Accepts `Confirm user presence for key <type> <fingerprint>`, optionally
/// followed by a line `public key authentication request for user "<user>"
/// to listed host` as ssh-agent adds it. An unknown second line is ignored.
/// Returns `None` for any other message.
#[must_use]
pub fn parse_prompt(message: &str) -> Option<PromptInfo> {
    let mut lines = message.split('\n');
    let mut fields = lines.next()?.strip_prefix(PRESENCE_PREFIX)?.split(' ');
    let key_type = fields.next().filter(|s| !s.is_empty())?;
    let fingerprint = fields.next().filter(|s| !s.is_empty())?;
    if fields.next().is_some() {
        return None;
    }
    let user = lines.next().and_then(|line| {
        line.strip_prefix(DESTINATION_PREFIX)?
            .strip_suffix(DESTINATION_SUFFIX)
    });
    Some(PromptInfo {
        key_type: key_type.to_owned(),
        fingerprint: fingerprint.to_owned(),
        user: user.map(str::to_owned),
    })
}

/// What an askpass invocation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Report a user-presence notice to the daemon.
    Notify,
    /// Replace this process with the fallback askpass program.
    Fallback,
}

/// Returns the mode for an `SSH_ASKPASS_PROMPT` value: [`Mode::Notify`]
/// only for `none`.
#[must_use]
pub fn mode(prompt: Option<&OsStr>) -> Mode {
    if prompt == Some(OsStr::new(NOTIFY_PROMPT)) {
        Mode::Notify
    } else {
        Mode::Fallback
    }
}

/// Process environment that askpass reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// `SSH_ASKPASS_PROMPT`.
    pub prompt: Option<OsString>,
    /// `XDG_RUNTIME_DIR`; empty counts as unset.
    pub runtime_dir: Option<PathBuf>,
    /// [`FALLBACK_ENV`]; empty counts as unset.
    pub fallback: Option<OsString>,
}

impl Environment {
    /// Reads the environment of the current process.
    #[must_use]
    pub fn from_process() -> Self {
        let nonempty = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        Self {
            prompt: std::env::var_os(PROMPT_ENV),
            runtime_dir: nonempty("XDG_RUNTIME_DIR").map(PathBuf::from),
            fallback: nonempty(FALLBACK_ENV),
        }
    }
}

/// Failure of an askpass invocation.
#[derive(Debug, thiserror::Error)]
pub enum AskpassError {
    #[error(
        "no fallback askpass program; set {FALLBACK_ENV} to answer passphrase and confirmation prompts"
    )]
    NoFallback,
    #[error("cannot run the fallback askpass program {}", path.display())]
    Exec {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot start the async runtime")]
    Runtime(#[source] io::Error),
    #[error("cannot register signal handlers")]
    Signals(#[source] io::Error),
    #[error("askpass is not supported on this platform")]
    Unsupported,
}

/// Runs askpass with `args`, the arguments OpenSSH passed after the program
/// name, and the current process environment.
///
/// See [`run_with`].
///
/// # Errors
///
/// See [`run_with`].
pub fn run(args: &[OsString]) -> Result<ExitCode, AskpassError> {
    run_with(args, &Environment::from_process())
}

/// Runs askpass with `args` and `env`.
///
/// In [`Mode::Notify`] it reports the notice to the daemon helper socket,
/// waits for SIGTERM, SIGINT or SIGHUP, reports the end, and returns
/// success. An unreachable daemon is skipped silently. Nothing is written to
/// stdout. The wait is capped at ten minutes, after which the end is
/// reported as [`Outcome::TimedOut`](touchcue_core::Outcome::TimedOut).
///
/// In [`Mode::Fallback`] it replaces the process with the fallback program,
/// passing `args` and the environment unchanged except for [`FALLBACK_ENV`],
/// and returns only on failure.
///
/// # Errors
///
/// Returns [`AskpassError::NoFallback`] when a non-notice prompt arrives and
/// no fallback is set, [`AskpassError::Exec`] when the fallback cannot be
/// started, [`AskpassError::Runtime`] or [`AskpassError::Signals`] when the
/// notice wait cannot be set up, and [`AskpassError::Unsupported`] outside
/// Linux.
pub fn run_with(args: &[OsString], env: &Environment) -> Result<ExitCode, AskpassError> {
    match mode(env.prompt.as_deref()) {
        Mode::Notify => notify(args, env),
        Mode::Fallback => fallback(args, env),
    }
}

#[cfg(target_os = "linux")]
fn notify(args: &[OsString], env: &Environment) -> Result<ExitCode, AskpassError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(AskpassError::Runtime)?;
    let detail = args
        .first()
        .and_then(|message| parse_prompt(&message.to_string_lossy()))
        .and_then(|info| info.detail());
    let socket = env
        .runtime_dir
        .as_ref()
        .map(|dir| dir.join(crate::helper_socket::SOCKET_PATH));
    runtime.block_on(async {
        // Registered before connecting, so an early SIGTERM is not missed.
        let mut signals = linux::StopSignals::register()?;
        linux::report(socket.as_deref(), detail, signals.recv()).await;
        Ok::<_, AskpassError>(())
    })?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(target_os = "linux"))]
fn notify(_args: &[OsString], _env: &Environment) -> Result<ExitCode, AskpassError> {
    Err(AskpassError::Unsupported)
}

#[cfg(unix)]
fn fallback(args: &[OsString], env: &Environment) -> Result<ExitCode, AskpassError> {
    use std::os::unix::process::CommandExt as _;

    let program = env.fallback.as_ref().ok_or(AskpassError::NoFallback)?;
    let source = std::process::Command::new(program)
        .args(args)
        .env_remove(FALLBACK_ENV)
        .exec();
    Err(AskpassError::Exec {
        path: PathBuf::from(program),
        source,
    })
}

#[cfg(not(unix))]
fn fallback(_args: &[OsString], _env: &Environment) -> Result<ExitCode, AskpassError> {
    Err(AskpassError::Unsupported)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::time::Duration;

    use tokio::net::UnixStream;
    use tokio::signal::unix::{Signal, SignalKind, signal};
    use touchcue_core::Op;
    use touchcue_core::Outcome;
    use touchcue_core::helper::{Message, Origin};

    use super::AskpassError;
    use crate::helper_socket::{connect, send};

    /// Longest wait for the completion signal; bounds a notice whose ssh
    /// process exited without sending it.
    pub(super) const MAX_WAIT: Duration = Duration::from_secs(600);
    /// The only request of an askpass connection.
    const SEQ: u32 = 1;

    /// SIGTERM, SIGINT and SIGHUP, the signals that end a notice.
    pub(super) struct StopSignals {
        terminate: Signal,
        interrupt: Signal,
        hangup: Signal,
    }

    impl StopSignals {
        pub(super) fn register() -> Result<Self, AskpassError> {
            let register = |kind| signal(kind).map_err(AskpassError::Signals);
            Ok(Self {
                terminate: register(SignalKind::terminate())?,
                interrupt: register(SignalKind::interrupt())?,
                hangup: register(SignalKind::hangup())?,
            })
        }

        /// Waits for any of the signals; a closed stream counts as received.
        pub(super) async fn recv(&mut self) {
            tokio::select! {
                _ = self.terminate.recv() => {}
                _ = self.interrupt.recv() => {}
                _ = self.hangup.recv() => {}
            }
        }
    }

    /// Reports a notice with `detail` to the daemon at `socket`, waits for
    /// `done` or [`MAX_WAIT`], then reports the end and returns its outcome.
    ///
    /// A socket failure is recorded at debug level and drops the connection,
    /// so the wait happens with or without a daemon.
    pub(super) async fn report(
        socket: Option<&Path>,
        detail: Option<String>,
        done: impl Future<Output = ()>,
    ) -> Outcome {
        let mut stream = match socket {
            Some(path) => match connect(path).await {
                Ok(stream) => Some(stream),
                Err(error) => {
                    tracing::debug!(
                        error = &error as &dyn std::error::Error,
                        "daemon unreachable"
                    );
                    None
                }
            },
            None => None,
        };
        let start = Message::Start {
            origin: Origin::Askpass,
            seq: SEQ,
            op: Op::Auth,
            detail,
        };
        deliver(&mut stream, &start).await;
        let outcome = tokio::select! {
            () = done => Outcome::Touched,
            () = tokio::time::sleep(MAX_WAIT) => Outcome::TimedOut,
        };
        let end = Message::End {
            origin: Origin::Askpass,
            seq: SEQ,
            outcome,
        };
        deliver(&mut stream, &end).await;
        outcome
    }

    /// Writes `message` to `stream`, dropping the connection on failure.
    async fn deliver(stream: &mut Option<UnixStream>, message: &Message) {
        let Some(conn) = stream.as_mut() else {
            return;
        };
        if let Err(error) = send(conn, message).await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "cannot write to the daemon"
            );
            *stream = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_NOTICE: &str = "Confirm user presence for key ED25519-SK SHA256:abcdef\n\
                                public key authentication request for user \"git\" to listed host";

    fn info(key_type: &str, fingerprint: &str, user: Option<&str>) -> PromptInfo {
        PromptInfo {
            key_type: key_type.to_owned(),
            fingerprint: fingerprint.to_owned(),
            user: user.map(str::to_owned),
        }
    }

    #[test]
    fn parses_ssh_and_keygen_notice() {
        assert_eq!(
            parse_prompt("Confirm user presence for key ECDSA-SK SHA256:abc+/="),
            Some(info("ECDSA-SK", "SHA256:abc+/=", None))
        );
    }

    #[test]
    fn parses_agent_notice_with_destination() {
        assert_eq!(
            parse_prompt(AGENT_NOTICE),
            Some(info("ED25519-SK", "SHA256:abcdef", Some("git")))
        );
    }

    #[test]
    fn ignores_unknown_second_line() {
        assert_eq!(
            parse_prompt("Confirm user presence for key ED25519-SK MD5:aa:bb\nsomething else"),
            Some(info("ED25519-SK", "MD5:aa:bb", None))
        );
    }

    #[test]
    fn rejects_other_messages() {
        for message in [
            "",
            "Enter passphrase for key '/home/u/.ssh/id_ed25519_sk': ",
            "Allow use of key ED25519-SK?\nKey fingerprint SHA256:abc.",
            "Confirm user presence for key ED25519-SK",
            "Confirm user presence for key ED25519-SK SHA256:abc extra",
            "Confirm user presence for key  SHA256:abc",
        ] {
            assert_eq!(parse_prompt(message), None, "{message:?}");
        }
    }

    #[test]
    fn detail_is_sanitized_and_bounded() {
        let hostile = info("ED25519-SK", "SHA256:abc", Some("a\u{1b}[2J\u{202E}b"));
        assert_eq!(
            hostile.detail().as_deref(),
            Some("ED25519-SK SHA256:abc → user a [2J b")
        );
        let long = info("ED25519-SK", "SHA256:abc", Some(&"x".repeat(500)));
        assert_eq!(
            long.detail().map(|d| d.chars().count()),
            Some(MAX_DETAIL_CHARS)
        );
    }

    #[test]
    fn only_none_prompt_notifies() {
        assert_eq!(mode(Some(OsStr::new("none"))), Mode::Notify);
        assert_eq!(mode(Some(OsStr::new("confirm"))), Mode::Fallback);
        assert_eq!(mode(Some(OsStr::new(""))), Mode::Fallback);
        assert_eq!(mode(None), Mode::Fallback);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prompt_without_fallback_is_refused() {
        let env = Environment {
            prompt: Some("confirm".into()),
            ..Environment::default()
        };
        let result = run_with(&["Allow use of key?".into()], &env);
        assert!(
            matches!(result, Err(AskpassError::NoFallback)),
            "{result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::path::Path;
        use std::sync::Arc;

        use tokio::io::{AsyncBufReadExt as _, BufReader};
        use tokio::net::UnixListener;
        use tokio::sync::Notify;
        use touchcue_core::Op;
        use touchcue_core::helper::{Message, Origin, ParseError};

        use super::super::linux::{MAX_WAIT, report};
        use touchcue_core::Outcome;

        #[derive(Debug, thiserror::Error)]
        enum TestError {
            #[error(transparent)]
            Io(#[from] std::io::Error),
            #[error(transparent)]
            Parse(#[from] ParseError),
            #[error("connection closed")]
            Closed,
        }

        async fn read_message(
            lines: &mut tokio::io::Lines<BufReader<tokio::net::UnixStream>>,
        ) -> Result<Message, TestError> {
            let line = lines.next_line().await?.ok_or(TestError::Closed)?;
            Ok(Message::parse(&line)?)
        }

        #[tokio::test]
        async fn reports_start_and_end() -> Result<(), TestError> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("helper.sock");
            let listener = UnixListener::bind(&path)?;
            let done = Arc::new(Notify::new());
            let reporter = tokio::spawn({
                let path = path.clone();
                let done = Arc::clone(&done);
                async move {
                    let detail = Some("ED25519-SK SHA256:abc".to_owned());
                    report(Some(&path), detail, done.notified()).await
                }
            });
            let (conn, _) = listener.accept().await?;
            let mut lines = BufReader::new(conn).lines();
            assert_eq!(
                read_message(&mut lines).await?,
                Message::Start {
                    origin: Origin::Askpass,
                    seq: 1,
                    op: Op::Auth,
                    detail: Some("ED25519-SK SHA256:abc".to_owned()),
                }
            );
            done.notify_one();
            assert_eq!(
                read_message(&mut lines).await?,
                Message::End {
                    origin: Origin::Askpass,
                    seq: 1,
                    outcome: Outcome::Touched,
                }
            );
            assert!(matches!(reporter.await, Ok(Outcome::Touched)));
            Ok(())
        }

        #[tokio::test]
        async fn missing_daemon_still_waits_for_done() {
            let outcome = report(Some(Path::new("/nonexistent/helper.sock")), None, async {}).await;
            assert_eq!(outcome, Outcome::Touched);
        }

        #[tokio::test(start_paused = true)]
        async fn wait_is_bounded() {
            let started = tokio::time::Instant::now();
            let outcome = report(None, None, std::future::pending()).await;
            assert_eq!(outcome, Outcome::TimedOut);
            assert!(started.elapsed() >= MAX_WAIT);
        }
    }
}
