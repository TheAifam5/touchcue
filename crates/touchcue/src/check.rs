//! The `check` subcommand.

use std::io::{self, IsTerminal as _, Write};
#[cfg(target_os = "linux")]
use std::path::Path;

use tokio::runtime::Runtime;
#[cfg(target_os = "linux")]
use touchcue_appinfo::linux::Origin;
use touchcue_core::config::RequesterConfig;
#[cfg(target_os = "linux")]
use touchcue_core::skip::SkipList;
use touchcue_ui::Backend;

use crate::config::{self, ConfigPath};

/// Failure to produce the `check` report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot write the report to stdout")]
    Stdout(#[from] io::Error),
}

/// Prints the configuration status, desktop capabilities, FIDO devices, the
/// UI backend `run` would choose, the configured IPC endpoints and the gpg
/// setup, the number of hooks and the requester skip list with how it reads
/// this process, and returns whether touchcue is usable.
///
/// Usable means the configuration is valid and every FIDO device found can
/// be opened. No device present still counts as usable, since devices plugged
/// in later are picked up. The backend is chosen from the probed capabilities
/// the way `run` chooses it, without starting the UI.
///
/// # Errors
///
/// Returns [`Error::Stdout`] when stdout cannot be written.
#[tracing::instrument(skip_all, err)]
pub fn run(config_path: Option<&ConfigPath>, runtime: &Runtime) -> Result<bool, Error> {
    let stdout = io::stdout();
    let color = stdout.is_terminal();
    report(config_path, runtime, color, &mut stdout.lock())
}

fn report(
    config_path: Option<&ConfigPath>,
    runtime: &Runtime,
    color: bool,
    out: &mut impl Write,
) -> Result<bool, Error> {
    let config = match config::load(config_path) {
        Ok(loaded) => {
            match (config_path, loaded.found) {
                (Some(path), true) => writeln!(out, "config: {}: valid", path.path.display())?,
                (Some(path), false) => writeln!(
                    out,
                    "config: {}: not found, using defaults",
                    path.path.display()
                )?,
                (None, _) => writeln!(out, "config: no path, HOME is unset; using defaults")?,
            }
            Some(loaded.config)
        }
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "configuration rejected"
            );
            match config::render(&error, &config::report_handler(color)) {
                Ok(report) => write!(out, "config: error:\n{report}")?,
                Err(render) => {
                    tracing::debug!(
                        error = &render as &dyn std::error::Error,
                        "cannot render the configuration error"
                    );
                    writeln!(out, "config: error: {}", chain(&error))?;
                }
            }
            None
        }
    };
    let mut usable = config.is_some();

    let caps = runtime.block_on(touchcue_ui::probe());
    writeln!(
        out,
        "wayland: {}\nlayer-shell: {}\nnotifications: {}",
        yes_no(caps.wayland),
        yes_no(caps.layer_shell),
        yes_no(caps.notifications)
    )?;

    usable &= devices(out)?;

    match &config {
        Some(config) => {
            let backend = backend_name(touchcue_ui::choose(&config.output, &caps));
            tracing::debug!(backend, "chose backend");
            writeln!(out, "backend: {backend}")?;
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| !d.is_empty());
            writeln!(
                out,
                "ipc: json={} dbus={} maxbaz-socket={} runtime-dir={}",
                on_off(config.ipc.enabled),
                on_off(config.dbus.enabled),
                on_off(config.compat.maxbaz_socket.enabled),
                if runtime_dir {
                    "set"
                } else {
                    "unset, IPC disabled"
                }
            )?;
            gpg(out, config.sources.gpg.enabled, runtime)?;
            writeln!(out, "hooks: {}", config.hooks.len())?;
            requester(out, &config.requester)?;
        }
        None => writeln!(out, "backend: unknown, the configuration did not load")?,
    }
    Ok(usable)
}

/// Prints each FIDO device and whether its node opens read-only; returns
/// whether all of them do.
#[cfg(target_os = "linux")]
#[tracing::instrument(skip_all)]
fn devices(out: &mut impl Write) -> Result<bool, Error> {
    use touchcue_detect::linux::sysfs;

    use crate::linux::{SYS_ROOT, device_line, probe_node};

    let devices = sysfs::list_fido(Path::new(SYS_ROOT));
    if devices.is_empty() {
        writeln!(out, "devices: none found")?;
    }
    let mut all_open = true;
    for device in &devices {
        match probe_node(&device.id.0) {
            Ok(()) => writeln!(out, "device: {}: readable", device_line(device))?,
            Err(error) => {
                tracing::debug!(error = &error as &dyn std::error::Error, path = %device.id.0, "cannot open device node");
                all_open = false;
                writeln!(
                    out,
                    "device: {}: not readable: {error}",
                    device_line(device)
                )?;
            }
        }
    }
    Ok(all_open)
}

/// Prints whether the daemon's helper socket accepts connections and
/// whether gpg-agent runs touchcue as its `scdaemon-program`.
#[cfg(target_os = "linux")]
#[tracing::instrument(skip_all)]
fn gpg(out: &mut impl Write, enabled: bool, runtime: &Runtime) -> Result<(), Error> {
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::net::UnixStream;

    if !enabled {
        writeln!(out, "gpg: helper socket disabled")?;
    } else if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let path = Path::new(&dir).join("touchcue").join("helper.sock");
        let up = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(&path) {
                Ok(_stream) => true,
                Err(error) => {
                    tracing::debug!(
                        error = &error as &dyn std::error::Error,
                        "helper socket refused"
                    );
                    false
                }
            },
            Ok(_) => false,
            Err(error) => {
                tracing::debug!(error = &error as &dyn std::error::Error, "no helper socket");
                false
            }
        };
        writeln!(
            out,
            "gpg: helper socket {}: {}",
            path.display(),
            if up { "listening" } else { "not running" }
        )?;
    } else {
        writeln!(
            out,
            "gpg: helper socket unavailable, XDG_RUNTIME_DIR is unset"
        )?;
    }
    let status = match runtime.block_on(touchcue_ipc::agent::paths()) {
        Ok(paths) => {
            let conf = paths.homedir.join(touchcue::gpg::CONF_FILE);
            match read_conf(&conf) {
                Ok(text) => scdaemon_status(text.as_deref()),
                Err(error) => {
                    tracing::debug!(error = &error as &dyn std::error::Error, path = %conf.display(), "cannot read gpg-agent.conf");
                    format!("unknown, cannot read {}: {error}", conf.display())
                }
            }
        }
        Err(error) => {
            tracing::debug!(error = &error as &dyn std::error::Error, "gpgconf failed");
            format!("unknown, {}", chain(&error))
        }
    };
    writeln!(out, "gpg: scdaemon-program: {status}")?;
    Ok(())
}

/// Largest `gpg-agent.conf` read, in bytes.
#[cfg(target_os = "linux")]
const MAX_CONF_BYTES: u64 = 64 * 1024;

/// Reads the first 64 KiB of `path`, or `None` when it does not exist.
#[cfg(target_os = "linux")]
fn read_conf(path: &Path) -> io::Result<Option<String>> {
    use std::io::Read as _;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONF_BYTES).read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Hint for a `scdaemon-program` line that names a missing or unusable touchcue.
#[cfg(target_os = "linux")]
const REINSTALL: &str = "run `touchcue gpg uninstall`, then `touchcue gpg install`";

/// Describes the last `scdaemon-program` line of `conf`, the one gpg-agent
/// uses, and whether a touchcue program it names is an executable file.
#[cfg(target_os = "linux")]
fn scdaemon_status(conf: Option<&str>) -> String {
    let Some(conf) = conf else {
        return "unset, no gpg-agent.conf; run `touchcue gpg install`".to_owned();
    };
    match touchcue::gpg::scdaemon_programs(conf).last() {
        Some(program) if Path::new(program).file_name() == Some("touchcue".as_ref()) => {
            let shown = touchcue_core::text::sanitize(program, 256).unwrap_or_default();
            match executable(Path::new(program)) {
                Ok(true) => "touchcue".to_owned(),
                Ok(false) => {
                    format!("touchcue at {shown}, which is not an executable file; {REINSTALL}")
                }
                Err(error) => {
                    tracing::debug!(
                        error = &error as &dyn std::error::Error,
                        "cannot inspect scdaemon-program"
                    );
                    format!("touchcue at {shown}, which cannot be found: {error}; {REINSTALL}")
                }
            }
        }
        Some(program) => format!(
            "another program, {}",
            touchcue_core::text::sanitize(program, 256).unwrap_or_default()
        ),
        None => "unset; run `touchcue gpg install`".to_owned(),
    }
}

/// Reports whether `path` is a regular file with an execute bit set.
#[cfg(target_os = "linux")]
fn executable(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta = std::fs::metadata(path)?;
    Ok(meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Prints the effective requester skip list as a TOML line, and how the
/// requester of this process reads: its walk with skipped names in brackets
/// and the resulting `requester.label`.
#[cfg(target_os = "linux")]
#[tracing::instrument(skip_all, err)]
fn requester(out: &mut impl Write, config: &RequesterConfig) -> Result<(), Error> {
    use touchcue_appinfo::linux::{Resolver, procfs};

    let list = config.skip_list();
    writeln!(out, "requester: {}", skip_line(config, &list))?;
    let pid = std::process::id();
    let origin = procfs::start_time(Path::new("/proc"), pid)
        .and_then(|start| Resolver::system().with_skip(list).origin(pid, start));
    writeln!(out, "requester: here: {}", here_line(origin.as_ref()))?;
    Ok(())
}

/// Returns `skip = [...]` listing `list`, followed by where it comes from.
#[cfg(target_os = "linux")]
fn skip_line(config: &RequesterConfig, list: &SkipList) -> String {
    let entries: Vec<String> = list.patterns().iter().map(|p| toml_string(p)).collect();
    let source = match (&config.skip, config.extend_skip.len()) {
        (None, 0) => "defaults".to_owned(),
        (None, added) => format!("defaults + {added} from extend_skip"),
        (Some(_), 0) => "replaced".to_owned(),
        (Some(_), added) => format!("replaced + {added} from extend_skip"),
    };
    format!("skip = [{}] ({source})", entries.join(", "))
}

/// Returns `text` as a TOML basic string.
#[cfg(target_os = "linux")]
fn toml_string(text: &str) -> String {
    let body: String = text
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            c if c.is_control() => format!("\\u{:04X}", u32::from(c)),
            c => c.to_string(),
        })
        .collect();
    format!("\"{body}\"")
}

/// Returns the walk of `origin`, client first, with skipped names in
/// brackets, then the requester label it yields.
#[cfg(target_os = "linux")]
fn here_line(origin: Option<&Origin>) -> String {
    let Some(origin) = origin else {
        return "unknown, this process cannot be read".to_owned();
    };
    let walk: Vec<String> = origin
        .walk
        .iter()
        .map(|step| {
            if step.skipped {
                format!("[{}]", step.name)
            } else {
                step.name.clone()
            }
        })
        .collect();
    let label = touchcue_core::placeholders::requester_label(
        origin.requester.as_ref().and_then(|r| r.name.as_deref()),
        origin.app.as_ref().and_then(|a| a.name.as_deref()),
    );
    match label {
        Some(label) => format!("{} → \"{label}\"", walk.join(" ← ")),
        None => format!("{} → no requester", walk.join(" ← ")),
    }
}

#[cfg(not(target_os = "linux"))]
fn requester(out: &mut impl Write, _config: &RequesterConfig) -> Result<(), Error> {
    writeln!(out, "requester: not supported on this platform yet")?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn gpg(out: &mut impl Write, _enabled: bool, _runtime: &Runtime) -> Result<(), Error> {
    writeln!(out, "gpg: not supported on this platform yet")?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn devices(out: &mut impl Write) -> Result<bool, Error> {
    writeln!(out, "devices: not supported on this platform yet")?;
    Ok(true)
}

/// Formats `error` followed by each of its sources, separated by `: `.
fn chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// Returns the lowercase name of a UI backend.
pub fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Popup => "popup",
        Backend::Notification => "notification",
        Backend::Off => "off",
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn scdaemon_status_checks_the_touchcue_program() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir()?;
        let program = dir.path().join("touchcue");
        std::fs::write(&program, "")?;
        let conf = format!("# x\nscdaemon-program {}\n", program.display());
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        assert_eq!(scdaemon_status(Some(&conf)), "touchcue");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644))?;
        assert!(
            scdaemon_status(Some(&conf))
                .contains("not an executable file; run `touchcue gpg uninstall`")
        );
        std::fs::remove_file(&program)?;
        assert!(scdaemon_status(Some(&conf)).contains("cannot be found"));
        Ok(())
    }

    #[test]
    fn skip_line_is_toml_and_names_its_source() -> Result<(), touchcue_core::ConfigError> {
        let config = touchcue_core::Config::from_toml(
            "[requester]\nskip = [\"make\", \"a\\\"b\"]\nextend_skip = [\"nvim\"]\n",
        )?;
        let requester = &config.requester;
        assert_eq!(
            skip_line(requester, &requester.skip_list()),
            r#"skip = ["make", "a\"b", "nvim"] (replaced + 1 from extend_skip)"#
        );
        let defaults = RequesterConfig::default();
        let line = skip_line(&defaults, &defaults.skip_list());
        assert!(line.starts_with("skip = [\"sh\", \"bash\""), "{line}");
        assert!(line.ends_with("\"init\"] (defaults)"), "{line}");
        let extended = RequesterConfig {
            skip: None,
            extend_skip: vec!["nvim".to_owned(), "x".to_owned()],
        };
        let line = skip_line(&extended, &extended.skip_list());
        assert!(
            line.ends_with("\"nvim\", \"x\"] (defaults + 2 from extend_skip)"),
            "{line}"
        );
        let empty = RequesterConfig {
            skip: Some(Vec::new()),
            extend_skip: Vec::new(),
        };
        assert_eq!(
            skip_line(&empty, &empty.skip_list()),
            "skip = [] (replaced)"
        );
        Ok(())
    }

    #[test]
    fn here_line_brackets_skipped_processes() {
        use touchcue_appinfo::linux::Step;
        use touchcue_core::{AppInfo, Requester};

        let step = |name: &str, skipped| Step {
            name: name.to_owned(),
            skipped,
        };
        let origin = Origin {
            app: Some(AppInfo {
                name: Some("kitty".to_owned()),
                ..AppInfo::default()
            }),
            requester: Some(Requester {
                name: Some("touchcue".to_owned()),
                exe: None,
                pid: 9,
            }),
            chain: None,
            walk: vec![
                step("touchcue", false),
                step("nu", true),
                step("herdr", true),
                step("kitty", false),
            ],
        };
        assert_eq!(
            here_line(Some(&origin)),
            "touchcue ← [nu] ← [herdr] ← kitty → \"touchcue in kitty\""
        );
        let none = Origin {
            app: None,
            requester: None,
            ..origin
        };
        assert_eq!(
            here_line(Some(&none)),
            "touchcue ← [nu] ← [herdr] ← kitty → no requester"
        );
        assert_eq!(here_line(None), "unknown, this process cannot be read");
    }

    #[test]
    fn scdaemon_status_names_the_configured_program() {
        assert_eq!(
            scdaemon_status(Some("scdaemon-program /usr/lib/scd\n")),
            "another program, /usr/lib/scd"
        );
        assert!(scdaemon_status(Some("pinentry-program x\n")).starts_with("unset"));
        assert!(scdaemon_status(None).starts_with("unset"));
    }
}
