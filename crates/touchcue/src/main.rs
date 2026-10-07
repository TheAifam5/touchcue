use std::env::{self, VarError};
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Runtime;
#[cfg(target_os = "linux")]
use touchcue::cli::GpgCommand;
use touchcue::cli::{Cli, Command};
use touchcue::{askpass, scdaemon};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

mod check;
mod config;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod icons;
#[cfg(target_os = "linux")]
mod linux;

/// File name under which touchcue runs as OpenSSH's `SSH_ASKPASS` program.
const ASKPASS_NAME: &str = "touchcue-askpass";
/// Start of every log line of the scdaemon wrapper.
const SCDAEMON_LOG_PREFIX: &str = "touchcue-scdaemon: ";
/// Environment variable holding a `tracing` filter that overrides `-v`.
const LOG_ENV: &str = "TOUCHCUE_LOG";
/// Worker threads of the async runtime; the daemon's work is I/O bound.
const WORKER_THREADS: usize = 2;
/// Most threads the runtime keeps for blocking work such as procfs scans.
const MAX_BLOCKING_THREADS: usize = 4;
/// Longest wait for remaining runtime tasks after the command finished.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> Result<ExitCode> {
    let mut command_line = env::args_os();
    let program = command_line.next();
    let args: Vec<OsString> = command_line.collect();
    // Checked before parsing: scdaemon's arguments are gpg-agent's, and an
    // askpass message is free text that must not be parsed as flags.
    if scdaemon::is_invocation(&args) {
        init_scdaemon_logging();
        return scdaemon::run(&args).context("scdaemon wrapper failed");
    }
    if program
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|name| name == ASKPASS_NAME)
    {
        return askpass::run(&args).context("askpass failed");
    }
    if let Some((first, message)) = args.split_first()
        && first == "askpass"
    {
        return askpass::run(message).context("askpass failed");
    }
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let config_path = config::path(
        cli.config,
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
    );
    match cli.command {
        Command::Check => {
            let runtime = runtime()?;
            let usable = check::run(config_path.as_ref(), &runtime);
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            Ok(if usable.context("check failed")? {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        #[cfg(target_os = "linux")]
        Command::Run => {
            let config = match config::load(config_path.as_ref()) {
                Ok(loaded) => loaded.config,
                Err(error) => return report_config_error(error),
            };
            let runtime = runtime()?;
            let result = runtime.block_on(linux::run(config));
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            result.context("daemon failed")?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(target_os = "linux")]
        Command::ListDevices => {
            linux::list_devices().context("cannot list devices")?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(target_os = "linux")]
        Command::Trace => {
            let runtime = runtime()?;
            let result = runtime.block_on(linux::trace());
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            result.context("trace failed")?;
            Ok(ExitCode::SUCCESS)
        }
        #[cfg(target_os = "linux")]
        Command::Gpg(gpg) => {
            let runtime = runtime()?;
            let result = runtime.block_on(async {
                match gpg.command {
                    GpgCommand::Install => touchcue::gpg::install().await,
                    GpgCommand::Uninstall(uninstall) => {
                        touchcue::gpg::uninstall(uninstall.restore_backup).await
                    }
                }
            });
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
            result.context("gpg setup failed")?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Askpass(command) => {
            let message: Vec<OsString> = command.message.into_iter().map(OsString::from).collect();
            askpass::run(&message).context("askpass failed")
        }
        #[cfg(not(target_os = "linux"))]
        Command::Run | Command::ListDevices | Command::Trace | Command::Gpg(_) => {
            eprintln!("touchcue: not supported on this platform yet");
            Ok(ExitCode::from(2))
        }
    }
}

/// Prints `error` as a graphical report on stderr, with colors only on a
/// terminal, and returns the failure exit code. Falls back to the plain
/// error chain when the report cannot be rendered or written.
#[cfg(target_os = "linux")]
fn report_config_error(error: config::LoadError) -> Result<ExitCode> {
    use std::io::Write as _;

    let stderr = std::io::stderr();
    let handler = config::report_handler(stderr.is_terminal());
    let report = match config::render(&error, &handler) {
        Ok(report) => report,
        Err(render) => {
            tracing::debug!(
                error = &render as &dyn std::error::Error,
                "cannot render the configuration error"
            );
            return Err(error).context("cannot load the configuration");
        }
    };
    if let Err(write) = stderr.lock().write_all(report.as_bytes()) {
        tracing::debug!(
            error = &write as &dyn std::error::Error,
            "cannot write the configuration error"
        );
        return Err(error).context("cannot load the configuration");
    }
    Ok(ExitCode::FAILURE)
}

/// Builds the multi-threaded runtime that runs `run`, `trace` and `check`.
///
/// `shutdown_timeout` after the command does not wait for blocking tasks
/// past its deadline; the process exits with them.
fn runtime() -> Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("touchcue-rt")
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}

/// Logs to stderr at `warn`, `info` for `-v` or `debug` for `-vv`, unless
/// `TOUCHCUE_LOG` holds a valid filter. An invalid filter falls back to the
/// verbosity default with a message on stderr, since logging is not set up yet.
fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let result = tracing_subscriber::fmt()
        .with_env_filter(log_filter(default))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
    if let Err(error) = result {
        eprintln!("touchcue: cannot initialize logging: {error}");
    }
}

/// Logs the scdaemon wrapper's warnings to stderr, unless `TOUCHCUE_LOG`
/// holds a valid filter.
///
/// gpg-agent passes its stderr to scdaemon unless it runs detached
/// (`--daemon`), where scdaemon's stderr is `/dev/null`. Under systemd the
/// lines reach the journal of `gpg-agent.service`, so every line is prefixed
/// with [`SCDAEMON_LOG_PREFIX`] and carries no colors or timestamps.
fn init_scdaemon_logging() {
    let format = tracing_subscriber::fmt::format()
        .without_time()
        .with_ansi(false);
    let result = tracing_subscriber::fmt()
        .with_env_filter(log_filter("warn"))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .event_format(Prefixed(format))
        .try_init();
    if let Err(error) = result {
        eprintln!("{SCDAEMON_LOG_PREFIX}cannot initialize logging: {error}");
    }
}

/// Returns the filter in `TOUCHCUE_LOG`, or `default` when it is unset or
/// invalid, saying so on stderr.
fn log_filter(default: &str) -> EnvFilter {
    match env::var(LOG_ENV) {
        Ok(directives) => match EnvFilter::try_new(&directives) {
            Ok(filter) => filter,
            Err(error) => {
                eprintln!("touchcue: ignoring invalid {LOG_ENV}: {error}");
                EnvFilter::new(default)
            }
        },
        Err(VarError::NotPresent) => EnvFilter::new(default),
        Err(VarError::NotUnicode(_)) => {
            eprintln!("touchcue: ignoring {LOG_ENV}, it is not valid UTF-8");
            EnvFilter::new(default)
        }
    }
}

/// Event format that starts every line with [`SCDAEMON_LOG_PREFIX`].
struct Prefixed<F>(F);

impl<S, N, F> FormatEvent<S, N> for Prefixed<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        writer.write_str(SCDAEMON_LOG_PREFIX)?;
        self.0.format_event(ctx, writer, event)
    }
}
