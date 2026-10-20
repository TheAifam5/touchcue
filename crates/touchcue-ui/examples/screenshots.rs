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
//! An animated scenario, named `anim-*`, starts its state changes when a
//! line arrives on stdin after `ready`, times them from that line, prints
//! `done` once the last change has lingered for [`ANIM_TAIL`], and then
//! holds the prompts the same way.
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

    use tokio::io::{AsyncReadExt as _, Stdin};
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
    /// Delay between prompt shows. The icon loader skips new icon loads while busy.
    const SHOW_GAP: Duration = Duration::from_millis(300);
    /// Delay between the final show/update command and `ready`.
    const SETTLE: Duration = Duration::from_millis(500);
    /// Time an animated scenario holds its last state before `done`.
    const ANIM_TAIL: Duration = Duration::from_millis(1500);
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

    const TOUCHED: RequestState = RequestState::Lingering(EndReason::Touched);
    const CANCELLED: RequestState = RequestState::Lingering(EndReason::Cancelled);
    const TIMED_OUT: RequestState = RequestState::Lingering(EndReason::TimedOut);

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
        #[error("scenario {0} must have steps exactly when its name starts with anim-")]
        AnimatedName(&'static str),
        #[error("nothing released the prompts within {limit:?}")]
        HoldTimeout {
            limit: Duration,
            #[source]
            source: tokio::time::error::Elapsed,
        },
    }

    /// One prompt of a scenario; it is shown waiting and then moved to
    /// `end`, when set, before `ready`.
    struct Shot {
        title: &'static str,
        body: &'static str,
        /// File name in `examples/assets`.
        icon: Option<&'static str>,
        end: Option<EndReason>,
    }

    /// A state change of an animated scenario: `after` the previous change,
    /// or the start line, every prompt moves to `state`.
    struct Step {
        after: Duration,
        state: RequestState,
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
        /// State changes played after the start line; empty for stills.
        steps: &'static [Step],
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

    const fn step(after_ms: u64, state: RequestState) -> Step {
        Step {
            after: Duration::from_millis(after_ms),
            state,
        }
    }

    /// The prompt of every animated scenario.
    const ANIMATED_SHOTS: &[Shot] = &[waiting(BROWSER, "browser.svg")];

    /// An animated popup of the browser prompt that plays `steps`.
    const fn animated(name: &'static str, steps: &'static [Step]) -> Scenario {
        Scenario {
            name,
            toml: POPUP_ONLY,
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: ANIMATED_SHOTS,
            steps,
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
            steps: &[],
        },
        Scenario {
            name: "corner",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nposition = \"top-right\"\n",
            pin: true,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[waiting(SSH, "terminal.svg")],
            steps: &[],
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
            steps: &[],
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
            steps: &[],
        },
        animated("anim-touched", &[step(1500, TOUCHED)]),
        animated("anim-cancelled", &[step(1500, CANCELLED)]),
        animated("anim-timed-out", &[step(1500, TIMED_OUT)]),
        animated(
            "anim-retry",
            &[
                step(1200, TIMED_OUT),
                step(1500, RequestState::Waiting),
                step(1500, TOUCHED),
            ],
        ),
        Scenario {
            name: "modal-dim",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nmodal = true\n",
            pin: true,
            backend: Backend::Popup,
            namespace: MODAL_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
            steps: &[],
        },
        Scenario {
            name: "modal-nodim",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\nmodal = true\nmodal_dim = 0.0\n",
            pin: true,
            backend: Backend::Popup,
            namespace: MODAL_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
            steps: &[],
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
            steps: &[],
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
            steps: &[],
        },
        Scenario {
            name: "all-outputs",
            toml: "[output]\nmode = \"popup\"\nfallback = \"none\"\n\
                   [popup]\noutput = \"all\"\n",
            pin: false,
            backend: Backend::Popup,
            namespace: POPUP_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
            steps: &[],
        },
        Scenario {
            name: "notification",
            toml: "[output]\nmode = \"notification\"\nfallback = \"none\"\n",
            pin: false,
            backend: Backend::Notification,
            namespace: MAKO_NAMESPACE,
            shots: &[waiting(BROWSER, "browser.svg")],
            steps: &[],
        },
    ];

    /// What released the held prompts.
    #[derive(Debug, Clone, Copy)]
    enum Release {
        StdinClosed,
        Terminated,
        Interrupted,
    }

    /// Sources that release the held prompts: stdin and two signal streams.
    struct Releases {
        stdin: Stdin,
        terminate: Signal,
        interrupt: Signal,
    }

    impl Releases {
        /// Waits for input on stdin, returning `None`, or for a release.
        async fn next(&mut self) -> Result<Option<Release>, ShotError> {
            let mut buf = [0_u8; 256];
            tokio::select! {
                read = self.stdin.read(&mut buf) => match read {
                    Ok(0) => Ok(Some(Release::StdinClosed)),
                    Ok(_) => Ok(None),
                    Err(error) => Err(ShotError::Stdin(error)),
                },
                _ = self.terminate.recv() => Ok(Some(Release::Terminated)),
                _ = self.interrupt.recv() => Ok(Some(Release::Interrupted)),
            }
        }

        /// Waits for a release; input on stdin is ignored.
        async fn released(&mut self) -> Result<Release, ShotError> {
            loop {
                if let Some(release) = self.next().await? {
                    return Ok(release);
                }
            }
        }
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
        // Pending stdin reads are not cancelable; runtime shutdown does not wait for them.
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

    /// Prints every scenario name after checking that its configuration
    /// parses and that only animated scenarios are named `anim-*`.
    fn list() -> Result<(), ShotError> {
        let mut stdout = io::stdout().lock();
        for scenario in SCENARIOS {
            ui_config(scenario, DEFAULT_OUTPUT)?;
            if scenario.name.starts_with("anim-") == scenario.steps.is_empty() {
                return Err(ShotError::AnimatedName(scenario.name));
            }
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
        popup.animations = scenario.name.starts_with("anim-");
        if scenario.pin {
            popup.output = OutputTarget::Named(vec![output.to_owned()]);
        }
        Ok(UiConfig {
            output: config.output,
            popup,
            notification: config.notification,
            reduced_motion: false,
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
        // Signal streams include signals received during UI startup.
        let releases = Releases {
            stdin: tokio::io::stdin(),
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
        let held = present(&ui, scenario, &assets, &ids, releases).await;
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
        releases: Releases,
    ) -> Result<Release, ShotError> {
        show(ui, scenario, assets, ids).await?;
        say(format_args!("ready {}", scenario.namespace))?;
        hold(ui, scenario, assets, ids, releases).await
    }

    fn prompt(assets: &Path, id: RequestId, shot: &Shot, state: RequestState) -> Prompt {
        Prompt {
            id,
            title: shot.title.to_owned(),
            body: shot.body.to_owned(),
            icon: shot.icon.map(|name| -> PathBuf { assets.join(name) }),
            state,
        }
    }

    /// Shows every prompt waiting, then moves the ended ones to their outcome.
    async fn show(
        ui: &Ui,
        scenario: &Scenario,
        assets: &Path,
        ids: &[RequestId],
    ) -> Result<(), ShotError> {
        for (&id, shot) in ids.iter().zip(scenario.shots) {
            ui.send(Command::Show(prompt(
                assets,
                id,
                shot,
                RequestState::Waiting,
            )))
            .await?;
            tokio::time::sleep(SHOW_GAP).await;
        }
        for (&id, shot) in ids.iter().zip(scenario.shots) {
            if let Some(end) = shot.end {
                let state = RequestState::Lingering(end);
                ui.send(Command::Update(prompt(assets, id, shot, state)))
                    .await?;
            }
        }
        tokio::time::sleep(SETTLE).await;
        Ok(())
    }

    fn say(line: std::fmt::Arguments<'_>) -> Result<(), ShotError> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{line}").map_err(ShotError::Stdout)?;
        stdout.flush().map_err(ShotError::Stdout)
    }

    /// Moves every prompt through the steps of the scenario, timed from the
    /// call, then waits [`ANIM_TAIL`].
    #[tracing::instrument(skip_all, fields(scenario = scenario.name), err)]
    async fn play(
        ui: &Ui,
        scenario: &Scenario,
        assets: &Path,
        ids: &[RequestId],
    ) -> Result<(), ShotError> {
        let mut at = Instant::now();
        for step in scenario.steps {
            at += step.after;
            tokio::time::sleep_until(at).await;
            for (&id, shot) in ids.iter().zip(scenario.shots) {
                ui.send(Command::Update(prompt(assets, id, shot, step.state)))
                    .await?;
            }
        }
        tokio::time::sleep_until(at + ANIM_TAIL).await;
        Ok(())
    }

    /// Plays an animated scenario once a line arrives on stdin and prints
    /// `done`, then waits until stdin closes, `SIGTERM` or `SIGINT` arrives,
    /// or [`HOLD_LIMIT`] passes.
    #[tracing::instrument(skip_all, fields(scenario = scenario.name), err)]
    async fn hold(
        ui: &Ui,
        scenario: &Scenario,
        assets: &Path,
        ids: &[RequestId],
        mut releases: Releases,
    ) -> Result<Release, ShotError> {
        let wait = async {
            if !scenario.steps.is_empty() {
                if let Some(release) = releases.next().await? {
                    return Ok(release);
                }
                tokio::select! {
                    played = play(ui, scenario, assets, ids) => played?,
                    release = releases.released() => return release,
                }
                say(format_args!("done"))?;
            }
            releases.released().await
        };
        match tokio::time::timeout(HOLD_LIMIT, wait).await {
            Ok(release) => release,
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_animated_scenarios_enable_animations() -> Result<(), ShotError> {
            for scenario in SCENARIOS {
                let config = ui_config(scenario, DEFAULT_OUTPUT)?;
                assert_eq!(config.popup.animations, scenario.name.starts_with("anim-"));
                assert!(!config.reduced_motion);
            }
            Ok(())
        }
    }
}
