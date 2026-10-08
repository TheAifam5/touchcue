//! Shows one built-in screenshot scenario of the popup or notification UI
//! on the current Wayland display, for `scripts/screenshots.sh`.
//!
//! `screenshots --list` prints the scenario names, one per line, after
//! checking that every scenario configuration parses; it opens no display.
//! `screenshots <scenario>` shows the scenario's prompts, prints
//! `ready <namespace>` on stdout, where `<namespace>` is the layer-shell
//! namespace to wait for, then holds them until stdin closes, `SIGTERM` or
//! `SIGINT` arrives, or [`HOLD_LIMIT`] passes, and withdraws them.
//!
//! Popups are pinned to the output named by `TOUCHCUE_SCREENSHOT_OUTPUT`,
//! `SHOT` by default, except where a scenario targets every output.
//! `TOUCHCUE_LOG` sets the log filter, `warn` by default; logs go to stderr.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("screenshots: only Linux is supported");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), linux::ShotError> {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::env::{self, VarError};
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio::io::AsyncReadExt as _;
    use tokio::signal::unix::{Signal, SignalKind, signal};
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;
    use touchcue_core::config::OutputTarget;
    use touchcue_core::{Config, ConfigError, EndReason, RequestId, RequestState};
    use touchcue_ui::{Backend, Command, Prompt, SHUTDOWN_TIMEOUT, Ui, UiConfig, UiError};
    use tracing::{error, info, instrument};
    use tracing_subscriber::EnvFilter;

    /// Environment variable naming the output that shows pinned popups.
    const OUTPUT_ENV: &str = "TOUCHCUE_SCREENSHOT_OUTPUT";
    const DEFAULT_OUTPUT: &str = "SHOT";
    const LOG_ENV: &str = "TOUCHCUE_LOG";
    /// Longest time the prompts are held before the example gives up.
    const HOLD_LIMIT: Duration = Duration::from_secs(120);
    /// Pause between two shows, so one icon decode ends before the next
    /// starts; the icon loader skips icons while it is busy.
    const SHOW_GAP: Duration = Duration::from_millis(300);
    /// Pause after the last command, so every popup is drawn before `ready`.
    const SETTLE: Duration = Duration::from_millis(500);
    /// Layer-shell namespace of popups; matches the UI's Wayland backend.
    const POPUP_NAMESPACE: &str = "touchcue";
    /// Layer-shell namespace of modal overlays; matches the UI's Wayland backend.
    const MODAL_NAMESPACE: &str = "touchcue-modal";
    /// Layer-shell namespace of mako, the notification server the script runs.
    const MAKO_NAMESPACE: &str = "notifications";

    const WAITING_TITLE: &str = "Touch your security key";
    const BROWSER: &str = "Browser is waiting for a passkey";
    const SSH: &str = "ssh in Terminal is waiting for an SSH login";
    const AGENT: &str = "claude in Terminal is waiting for a GPG signature";

    /// A screenshot example failure.
    #[derive(Debug, thiserror::Error)]
    pub(super) enum ShotError {
        #[error("usage: screenshots <scenario> | --list")]
        Usage,
        #[error("unknown scenario {0:?}; run with --list")]
        UnknownScenario(String),
        #[error("the configuration of scenario {scenario} is invalid")]
        Config {
            scenario: &'static str,
            #[source]
            source: ConfigError,
        },
        #[error("failed to start the async runtime")]
        Runtime(#[source] io::Error),
        #[error("{OUTPUT_ENV} is not valid UTF-8")]
        OutputName,
        #[error(transparent)]
        Ui(#[from] UiError),
        #[error("the UI chose backend {actual:?} instead of {expected:?}")]
        Backend { expected: Backend, actual: Backend },
        #[error("failed to write to stdout")]
        Stdout(#[source] io::Error),
        #[error("failed to read stdin")]
        Stdin(#[source] io::Error),
        #[error("failed to listen for signals")]
        Signal(#[source] io::Error),
        #[error("nothing released the prompts within {limit:?}")]
        HoldTimeout {
            limit: Duration,
            #[source]
            source: tokio::time::error::Elapsed,
        },
    }

    /// One prompt of a scenario; it is shown waiting and then moved to
    /// `end`, when set.
    struct Shot {
        title: &'static str,
        body: &'static str,
        /// File name in `examples/assets`.
        icon: Option<&'static str>,
        end: Option<EndReason>,
    }

    struct Scenario {
        name: &'static str,
        /// Configuration document; only `[output]`, `[popup]` and
        /// `[notification]` reach the UI.
        toml: &'static str,
        /// Pins popups to the screenshot output instead of `popup.output`.
        pin: bool,
        backend: Backend,
        namespace: &'static str,
        shots: &'static [Shot],
    }

    const fn waiting(body: &'static str, icon: &'static str) -> Shot {
        Shot {
            title: WAITING_TITLE,
            body,
            icon: Some(icon),
            end: None,
        }
    }

    const fn ended(body: &'static str, icon: &'static str, end: EndReason) -> Shot {
        Shot {
            end: Some(end),
            ..waiting(body, icon)
        }
    }

    const POPUP_ONLY: &str = "[output]\nmode = \"popup\"\nfallback = \"none\"\n";

    const SCENARIOS: &[Scenario] = &[
        Scenario {
            name: "default",
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
        },
        Scenario {
            name: "corner",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nposition = \"top-right\"\n",
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[waiting(SSH, "terminal.svg")],
        },
        Scenario {
            name: "stacked",
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[
                waiting(BROWSER, "browser.svg"),
                ended(AGENT, "terminal.svg", EndReason::Touched),
            ],
        },
        Scenario {
            name: "outcomes",
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[
                ended(SSH, "terminal.svg", EndReason::Touched),
                ended(BROWSER, "browser.svg", EndReason::Cancelled),
                ended(AGENT, "terminal.svg", EndReason::TimedOut),
            ],
        },
        Scenario {
            name: "modal-dim",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nmodal = true\n",
            pin: true,
            backend: Backend::Popup,
            namespace: MODAL_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
        },
        Scenario {
            name: "modal-nodim",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nmodal = true\nmodal_dim = 0.0\n",
            pin: true,
            backend: Backend::Popup,
            namespace: MODAL_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
        },
        Scenario {
            name: "long-text",
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[Shot {
                title: "Touch your security key to approve the release signature \
                        requested by the build container running in the background",
                body: "release-tool in Terminal is waiting for a GPG signature on \
                       the tag of version 4.2.0 of the example project, its source \
                       archive, the checksum file and the detached signatures of \
                       every binary built for the six supported targets, which \
                       together make a body long enough to reach the line limit \
                       of the popup so that the end of this text is cut off",
                icon: Some("terminal.svg"),
                end: None,
            }],
        },
        Scenario {
            name: "rule-icon",
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[
                waiting(SSH, "terminal.svg"),
                Shot {
                    title: "Touch your key to sign the commit",
                    body: "git in Terminal is waiting for a GPG signature",
                    icon: Some("key.svg"),
                    end: None,
                },
            ],
        },
        Scenario {
            name: "all-outputs",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\noutput = \"all\"\n",
            pin: false,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
        },
        Scenario {
            name: "notification",
            toml: "[output]\nmode = \"notification\"\nfallback = \"none\"\n",
            pin: false,
            backend: Backend::Notification,
            namespace: MAKO_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
        },
    ];

    /// What released the held prompts.
    #[derive(Debug, Clone, Copy)]
    enum Release {
        StdinClosed,
        Terminated,
        Interrupted,
    }

    /// Signal streams that release the held prompts.
    struct Signals {
        terminate: Signal,
        interrupt: Signal,
    }

    pub(super) fn main() -> Result<(), ShotError> {
        init_logging();
        let mut args = env::args_os().skip(1);
        let (Some(arg), None) = (args.next(), args.next()) else {
            return Err(ShotError::Usage);
        };
        let Some(arg) = arg.to_str() else {
            return Err(ShotError::Usage);
        };
        if arg == "--list" {
            return list();
        }
        let scenario = SCENARIOS
            .iter()
            .find(|scenario| scenario.name == arg)
            .ok_or_else(|| ShotError::UnknownScenario(arg.to_owned()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ShotError::Runtime)?;
        let result = runtime.block_on(run(scenario));
        // A pending stdin read cannot be cancelled and would block a waiting shutdown.
        runtime.shutdown_background();
        result
    }

    /// Logs to stderr with the filter in `TOUCHCUE_LOG`, or `warn` when it
    /// is unset or invalid.
    fn init_logging() {
        let filter = match env::var(LOG_ENV) {
            Ok(directives) => match EnvFilter::try_new(&directives) {
                Ok(filter) => filter,
                Err(error) => {
                    eprintln!("screenshots: ignoring invalid {LOG_ENV}: {error}");
                    EnvFilter::new("warn")
                }
            },
            Err(VarError::NotPresent) => EnvFilter::new("warn"),
            Err(VarError::NotUnicode(_)) => {
                eprintln!("screenshots: ignoring {LOG_ENV}, it is not valid UTF-8");
                EnvFilter::new("warn")
            }
        };
        let result = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .with_ansi(false)
            .try_init();
        if let Err(error) = result {
            eprintln!("screenshots: cannot initialize logging: {error}");
        }
    }

    /// Prints every scenario name after checking that its configuration parses.
    fn list() -> Result<(), ShotError> {
        let mut stdout = io::stdout().lock();
        for scenario in SCENARIOS {
            ui_config(scenario, DEFAULT_OUTPUT)?;
            writeln!(stdout, "{}", scenario.name).map_err(ShotError::Stdout)?;
        }
        stdout.flush().map_err(ShotError::Stdout)
    }

    fn ui_config(scenario: &Scenario, output: &str) -> Result<UiConfig, ShotError> {
        let config = Config::from_toml(scenario.toml).map_err(|source| ShotError::Config {
            scenario: scenario.name,
            source,
        })?;
        let mut popup = config.popup;
        if scenario.pin {
            popup.output = OutputTarget::Named(vec![output.to_owned()]);
        }
        Ok(UiConfig {
            output: config.output,
            popup,
            notification: config.notification,
        })
    }

    fn output_name() -> Result<String, ShotError> {
        match env::var(OUTPUT_ENV) {
            Ok(name) => Ok(name),
            Err(VarError::NotPresent) => Ok(DEFAULT_OUTPUT.to_owned()),
            Err(VarError::NotUnicode(_)) => Err(ShotError::OutputName),
        }
    }

    #[instrument(skip_all, fields(scenario = scenario.name), err)]
    async fn run(scenario: &Scenario) -> Result<(), ShotError> {
        let cfg = ui_config(scenario, &output_name()?)?;
        // Registered first, so a signal during startup is not fatal and
        // still reaches `hold`.
        let signals = Signals {
            terminate: signal(SignalKind::terminate()).map_err(ShotError::Signal)?,
            interrupt: signal(SignalKind::interrupt()).map_err(ShotError::Signal)?,
        };
        let ui = Ui::spawn(cfg, CancellationToken::new()).await?;
        if ui.backend() != scenario.backend {
            return Err(ShotError::Backend {
                expected: scenario.backend,
                actual: ui.backend(),
            });
        }
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/assets");
        let ids: Vec<RequestId> = (1..).map(RequestId).take(scenario.shots.len()).collect();
        let held = present(&ui, scenario, &assets, &ids, signals).await;
        let withdrawn = withdraw(ui, &ids).await;
        match (held, withdrawn) {
            (Ok(release), withdrawn) => {
                info!(?release, "prompts released");
                withdrawn
            }
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup)) => {
                error!(
                    error = &cleanup as &dyn std::error::Error,
                    "failed to withdraw the prompts"
                );
                Err(error)
            }
        }
    }

    /// Shows the prompts, announces them on stdout and holds them.
    async fn present(
        ui: &Ui,
        scenario: &Scenario,
        assets: &Path,
        ids: &[RequestId],
        signals: Signals,
    ) -> Result<Release, ShotError> {
        show(ui, scenario, assets, ids).await?;
        ready(scenario.namespace)?;
        hold(signals).await
    }

    /// Shows every prompt waiting, then moves the ended ones to their outcome.
    async fn show(
        ui: &Ui,
        scenario: &Scenario,
        assets: &Path,
        ids: &[RequestId],
    ) -> Result<(), ShotError> {
        let prompt = |id: RequestId, shot: &Shot, state| Prompt {
            id,
            title: shot.title.to_owned(),
            body: shot.body.to_owned(),
            icon: shot.icon.map(|name| -> PathBuf { assets.join(name) }),
            state,
        };
        for (&id, shot) in ids.iter().zip(scenario.shots) {
            ui.send(Command::Show(prompt(id, shot, RequestState::Waiting)))
                .await?;
            tokio::time::sleep(SHOW_GAP).await;
        }
        for (&id, shot) in ids.iter().zip(scenario.shots) {
            if let Some(end) = shot.end {
                let state = RequestState::Lingering(end);
                ui.send(Command::Update(prompt(id, shot, state))).await?;
            }
        }
        tokio::time::sleep(SETTLE).await;
        Ok(())
    }

    fn ready(namespace: &str) -> Result<(), ShotError> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "ready {namespace}").map_err(ShotError::Stdout)?;
        stdout.flush().map_err(ShotError::Stdout)
    }

    /// Waits until stdin closes, `SIGTERM` or `SIGINT` arrives, or
    /// [`HOLD_LIMIT`] passes.
    async fn hold(signals: Signals) -> Result<Release, ShotError> {
        let Signals {
            mut terminate,
            mut interrupt,
        } = signals;
        let mut stdin = tokio::io::stdin();
        let mut buf = [0_u8; 256];
        let wait = async {
            loop {
                tokio::select! {
                    read = stdin.read(&mut buf) => match read {
                        Ok(0) => return Ok(Release::StdinClosed),
                        Ok(_) => {}
                        Err(error) => return Err(ShotError::Stdin(error)),
                    },
                    _ = terminate.recv() => return Ok(Release::Terminated),
                    _ = interrupt.recv() => return Ok(Release::Interrupted),
                }
            }
        };
        match tokio::time::timeout(HOLD_LIMIT, wait).await {
            Ok(released) => released,
            Err(source) => Err(ShotError::HoldTimeout {
                limit: HOLD_LIMIT,
                source,
            }),
        }
    }

    /// Hides every prompt and stops the UI task within [`SHUTDOWN_TIMEOUT`].
    async fn withdraw(ui: Ui, ids: &[RequestId]) -> Result<(), ShotError> {
        let mut hidden = Ok(());
        for &id in ids {
            if let Err(error) = ui.send(Command::Hide(id)).await {
                hidden = Err(error);
                break;
            }
        }
        let stopped = ui.shutdown(Instant::now() + SHUTDOWN_TIMEOUT).await;
        match (hidden, stopped) {
            (Ok(()), stopped) => Ok(stopped?),
            (Err(error), Ok(())) => Err(error.into()),
            (Err(error), Err(stop)) => {
                error!(
                    error = &stop as &dyn std::error::Error,
                    "failed to stop the UI task"
                );
                Err(error.into())
            }
        }
    }
}
