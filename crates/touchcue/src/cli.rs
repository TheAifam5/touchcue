//! Command-line argument definitions.

use std::path::PathBuf;

/// Shows who is waiting for your security key touch
#[derive(Debug, usage::Cli)]
#[usage(
    bin = "touchcue",
    version,
    unknown_flags = "error",
    args_override_self = false
)]
pub struct Cli {
    #[usage(
        long,
        global,
        value_name = "PATH",
        value_hint = usage::ValueHint::FilePath,
        help = "Configuration file [default: $XDG_CONFIG_HOME/touchcue/config.toml]"
    )]
    pub config: Option<PathBuf>,
    #[usage(
        short = 'v',
        long,
        global,
        count,
        help = "Log more: -v for info, -vv for debug; TOUCHCUE_LOG overrides it"
    )]
    pub verbose: u8,
    #[usage(subcommand)]
    pub command: Command,
}

/// Subcommands of the `touchcue` binary.
#[derive(Debug, Clone, PartialEq, Eq, usage::Subcommands)]
pub enum Command {
    /// Run the daemon in the foreground
    Run,
    /// Check the configuration, desktop capabilities and devices
    Check,
    /// List the connected FIDO devices
    ListDevices,
    /// Print touch detection signals until interrupted
    Trace,
    /// Set up touch detection for gpg and ssh through gpg-agent
    Gpg(Gpg),
    /// Answer an OpenSSH askpass prompt
    Askpass(Askpass),
}

/// Answer an OpenSSH askpass prompt
///
/// OpenSSH runs the `SSH_ASKPASS` program with no extra arguments, so point
/// it at a link to touchcue named `touchcue-askpass`. `touchcue askpass`
/// passes every argument after it through unparsed.
#[derive(Debug, Clone, PartialEq, Eq, usage::Args)]
pub struct Askpass {
    #[usage(
        arg,
        value_name = "MESSAGE",
        double_dash = "automatic",
        help = "The prompt OpenSSH passes"
    )]
    pub message: Vec<String>,
}

/// Set up touch detection for gpg and ssh through gpg-agent
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::Args)]
pub struct Gpg {
    #[usage(subcommand)]
    pub command: GpgCommand,
}

/// Subcommands of `touchcue gpg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::Subcommands)]
pub enum GpgCommand {
    /// Make gpg-agent run touchcue as its scdaemon
    Install,
    /// Stop gpg-agent from running touchcue as its scdaemon
    Uninstall(Uninstall),
}

/// Stop gpg-agent from running touchcue as its scdaemon
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::Args)]
pub struct Uninstall {
    #[usage(
        long,
        help = "Replace gpg-agent.conf with the backup taken by install, then delete the backup"
    )]
    pub restore_backup: bool,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use super::*;

    fn parse(words: &[&'static str]) -> Result<Cli, usage::Error<'static, 'static>> {
        let argv: Vec<&OsStr> = words.iter().map(|word| OsStr::new(*word)).collect();
        Cli::parse_from(&argv)
    }

    #[test]
    fn each_subcommand_parses() -> Result<(), usage::Error<'static, 'static>> {
        for (name, command) in [
            ("run", Command::Run),
            ("check", Command::Check),
            ("list-devices", Command::ListDevices),
            ("trace", Command::Trace),
        ] {
            assert_eq!(parse(&[name])?.command, command);
        }
        Ok(())
    }

    #[test]
    fn askpass_takes_the_message_unparsed() -> Result<(), usage::Error<'static, 'static>> {
        assert_eq!(
            parse(&["askpass", "Confirm user presence\nfor key", "--help"])?.command,
            Command::Askpass(Askpass {
                message: vec![
                    "Confirm user presence\nfor key".to_owned(),
                    "--help".to_owned()
                ]
            })
        );
        Ok(())
    }

    #[test]
    fn gpg_subcommands_parse() -> Result<(), usage::Error<'static, 'static>> {
        let gpg = |command| Command::Gpg(Gpg { command });
        assert_eq!(
            parse(&["gpg", "install"])?.command,
            gpg(GpgCommand::Install)
        );
        assert_eq!(
            parse(&["gpg", "uninstall"])?.command,
            gpg(GpgCommand::Uninstall(Uninstall {
                restore_backup: false
            }))
        );
        assert_eq!(
            parse(&["gpg", "uninstall", "--restore-backup"])?.command,
            gpg(GpgCommand::Uninstall(Uninstall {
                restore_backup: true
            }))
        );
        assert!(matches!(
            parse(&["gpg"]),
            Err(usage::Error::MissingSubcommand)
        ));
        Ok(())
    }

    #[test]
    fn config_is_global() -> Result<(), usage::Error<'static, 'static>> {
        let before = parse(&["--config", "a.toml", "check"])?;
        let after = parse(&["check", "--config", "b.toml"])?;
        assert_eq!(before.config.as_deref(), Some(Path::new("a.toml")));
        assert_eq!(after.config.as_deref(), Some(Path::new("b.toml")));
        Ok(())
    }

    #[test]
    fn verbose_counts_occurrences() -> Result<(), usage::Error<'static, 'static>> {
        assert_eq!(parse(&["run"])?.verbose, 0);
        assert_eq!(parse(&["-vv", "run"])?.verbose, 2);
        assert_eq!(parse(&["-v", "run", "-v"])?.verbose, 2);
        Ok(())
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(matches!(
            parse(&["run", "--bogus"]),
            Err(usage::Error::UnknownFlag { .. })
        ));
    }

    #[test]
    fn missing_subcommand_is_rejected() {
        assert!(matches!(parse(&[]), Err(usage::Error::MissingSubcommand)));
        assert!(matches!(
            parse(&["-v"]),
            Err(usage::Error::MissingSubcommand)
        ));
    }

    #[test]
    fn help_and_version_are_reported() {
        assert!(matches!(
            parse(&["--help"]),
            Err(usage::Error::Help { long: true, .. })
        ));
        assert!(matches!(
            parse(&["run", "-h"]),
            Err(usage::Error::Help { long: false, .. })
        ));
        assert!(matches!(
            parse(&["--version"]),
            Err(usage::Error::Version { long: true })
        ));
        assert!(matches!(
            parse(&["-V"]),
            Err(usage::Error::Version { long: false })
        ));
    }
}
