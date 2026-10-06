//! `touchcue gpg install` and `touchcue gpg uninstall`: point gpg-agent's
//! `scdaemon-program` at touchcue, or take it back.
//!
//! Only `gpg-agent.conf` and its backup in the gpg home directory are read
//! or written. The text edits are pure functions; the commands around them
//! run `gpgconf` and touch the two files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Configuration file of gpg-agent inside the gpg home directory.
pub const CONF_FILE: &str = "gpg-agent.conf";
/// Copy of [`CONF_FILE`] taken by `install` before its first edit.
pub const BACKUP_FILE: &str = "gpg-agent.conf.touchcue-backup";
/// gpg-agent option naming the program it runs as scdaemon.
const OPTION: &str = "scdaemon-program";
/// File name of the touchcue binary, used to recognize its own line.
const PROGRAM_NAME: &str = "touchcue";

/// Why a configuration edit is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// `scdaemon-program` already names another program.
    #[error("gpg-agent.conf already sets scdaemon-program to {program}")]
    Foreign { program: String },
    /// The program path holds a control character, such as a newline, that
    /// would end or break the line.
    #[error("the touchcue path contains a control character: {program:?}")]
    ControlCharacter { program: String },
}

/// Returns the values of every `scdaemon-program` line in `conf`, in order.
#[must_use]
pub fn scdaemon_programs(conf: &str) -> Vec<&str> {
    conf.lines().filter_map(option_value).collect()
}

/// Returns `conf` with a `scdaemon-program <program>` line appended, or
/// `None` when it already names `program`.
///
/// # Errors
///
/// Returns [`EditError::ControlCharacter`] when `program` contains a control
/// character, and [`EditError::Foreign`] when a `scdaemon-program` line
/// names a different program.
pub fn add_program(conf: &str, program: &str) -> Result<Option<String>, EditError> {
    if program.chars().any(char::is_control) {
        return Err(EditError::ControlCharacter {
            program: program.to_owned(),
        });
    }
    let existing = scdaemon_programs(conf);
    if let Some(foreign) = existing.iter().find(|value| **value != program) {
        return Err(EditError::Foreign {
            program: (*foreign).to_owned(),
        });
    }
    if !existing.is_empty() {
        return Ok(None);
    }
    let mut edited = conf.to_owned();
    if !edited.is_empty() && !edited.ends_with('\n') {
        edited.push('\n');
    }
    edited.push_str(OPTION);
    edited.push(' ');
    edited.push_str(program);
    edited.push('\n');
    Ok(Some(edited))
}

/// Returns `conf` without its `scdaemon-program` lines that name touchcue,
/// or `None` when there is none. Every other line is kept byte for byte.
///
/// A line names touchcue when its value is `program` or a path whose file
/// name is `touchcue`.
///
/// # Errors
///
/// Returns [`EditError::Foreign`] when a `scdaemon-program` line names a
/// different program; nothing is removed then.
pub fn remove_program(conf: &str, program: &str) -> Result<Option<String>, EditError> {
    let names_touchcue = |value: &str| {
        value == program || Path::new(value).file_name() == Some(PROGRAM_NAME.as_ref())
    };
    let existing = scdaemon_programs(conf);
    if let Some(foreign) = existing.iter().find(|value| !names_touchcue(value)) {
        return Err(EditError::Foreign {
            program: (*foreign).to_owned(),
        });
    }
    if existing.is_empty() {
        return Ok(None);
    }
    let edited = conf
        .split_inclusive('\n')
        .filter(|line| option_value(line).is_none())
        .collect();
    Ok(Some(edited))
}

/// Returns `conf` without its `scdaemon-program` lines that name touchcue,
/// as [`remove_program`] does, or `conf` unchanged when there is none.
///
/// # Errors
///
/// Returns [`EditError::Foreign`] when a `scdaemon-program` line names a
/// different program.
pub fn without_program(conf: &str, program: &str) -> Result<String, EditError> {
    Ok(remove_program(conf, program)?.unwrap_or_else(|| conf.to_owned()))
}

/// Whether `path` lies in a directory that a version upgrade replaces: a
/// mise install, a cargo registry, or any directory whose name holds a
/// version number such as `1.2` or `v0.3.1`.
#[must_use]
pub fn looks_versioned(path: &Path) -> bool {
    let names: Vec<&OsStr> = path.parent().into_iter().flat_map(Path::iter).collect();
    let follows = |first: &str, second: &str| {
        names
            .windows(2)
            .any(|pair| matches!(pair, [a, b] if *a == first && *b == second))
    };
    follows("mise", "installs")
        || follows(".cargo", "registry")
        || names
            .iter()
            .any(|name| has_version(name.as_encoded_bytes()))
}

/// Whether `name` contains digits, a dot and digits.
fn has_version(name: &[u8]) -> bool {
    name.windows(3)
        .any(|window| matches!(window, [a, b'.', c] if a.is_ascii_digit() && c.is_ascii_digit()))
}

/// Returns the value of a `scdaemon-program` line, without surrounding
/// whitespace or double quotes.
fn option_value(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix(OPTION)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let value = rest.trim();
    Some(
        value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value),
    )
}

/// Decodes the `%XX` escapes `gpgconf --list-dirs` writes for `%`, `:` and
/// other special bytes. A malformed escape is kept as written.
#[must_use]
pub fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = match (byte, bytes.get(index + 1), bytes.get(index + 2)) {
            (b'%', Some(&high), Some(&low)) => hex_digit(high)
                .zip(hex_digit(low))
                .map(|(high, low)| high << 4 | low),
            _ => None,
        };
        if let Some(escaped) = escaped {
            decoded.push(escaped);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    decoded
}

/// Returns the value of an ASCII hex digit.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Failure of `gpg install`, `gpg uninstall` or a `gpgconf` query.
#[derive(Debug, thiserror::Error)]
pub enum GpgError {
    #[error("cannot run gpgconf {args}")]
    Spawn {
        args: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gpgconf {args} did not finish in time")]
    TimedOut {
        args: String,
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("gpgconf {args} failed with {status}")]
    Status {
        args: String,
        status: std::process::ExitStatus,
    },
    #[error("gpgconf {args} printed no path")]
    NoOutput { args: String },
    #[error("gpgconf {args} printed text that is not UTF-8")]
    NotUtf8 {
        args: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("cannot locate the touchcue executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("the touchcue path is not valid UTF-8: {}", path.display())]
    NonUtf8Path { path: PathBuf },
    #[error("cannot {action} {}", path.display())]
    File {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a file", path.display())]
    NotAFile { path: PathBuf },
    #[error("no backup at {}", path.display())]
    NoBackup { path: PathBuf },
    #[error(
        "{} was changed after install; restoring {} would discard those changes. \
         Run `touchcue gpg uninstall` and remove the backup instead",
        conf.display(),
        backup.display()
    )]
    RestoreDiscards { conf: PathBuf, backup: PathBuf },
    #[error(transparent)]
    Edit(#[from] EditError),
}

#[cfg(target_os = "linux")]
pub use linux::{gpgconf_dir, install, uninstall};

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File, OpenOptions};
    use std::io::{ErrorKind, Write as _};
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::process::Command;

    use super::{
        BACKUP_FILE, CONF_FILE, GpgError, OPTION, add_program, looks_versioned, percent_decode,
        remove_program, without_program,
    };

    /// Longest time one `gpgconf` call may take.
    const GPGCONF_TIMEOUT: Duration = Duration::from_secs(10);
    /// Mode of a configuration file or backup that touchcue creates.
    const FILE_MODE: u32 = 0o600;

    /// Returns the directory `gpgconf --list-dirs <name>` prints, for the
    /// gpg home directory `homedir` or the default one.
    ///
    /// # Errors
    ///
    /// Returns [`GpgError`] when gpgconf cannot run, fails, times out, or
    /// prints no path.
    pub async fn gpgconf_dir(homedir: Option<&OsStr>, name: &str) -> Result<PathBuf, GpgError> {
        let mut args: Vec<&OsStr> = Vec::new();
        if let Some(homedir) = homedir {
            args.extend([OsStr::new("--homedir"), homedir]);
        }
        args.extend([OsStr::new("--list-dirs"), OsStr::new(name)]);
        let output = gpgconf(&args).await?;
        let value = output.lines().next().unwrap_or_default().trim_end();
        if value.is_empty() {
            return Err(GpgError::NoOutput {
                args: joined(&args),
            });
        }
        Ok(PathBuf::from(OsString::from_vec(percent_decode(value))))
    }

    fn joined(args: &[&OsStr]) -> String {
        let words: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        words.join(" ")
    }

    /// Runs `gpgconf` with `args` and returns its standard output.
    #[tracing::instrument(skip_all, fields(args = %joined(args)))]
    async fn gpgconf(args: &[&OsStr]) -> Result<String, GpgError> {
        let output = Command::new("gpgconf")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(GPGCONF_TIMEOUT, output)
            .await
            .map_err(|source| GpgError::TimedOut {
                args: joined(args),
                source,
            })?
            .map_err(|source| GpgError::Spawn {
                args: joined(args),
                source,
            })?;
        if !output.status.success() {
            return Err(GpgError::Status {
                args: joined(args),
                status: output.status,
            });
        }
        String::from_utf8(output.stdout).map_err(|source| GpgError::NotUtf8 {
            args: joined(args),
            source,
        })
    }

    /// Makes gpg-agent run `program` as its scdaemon: backs up
    /// `gpg-agent.conf`, appends one `scdaemon-program` line, reloads
    /// gpg-agent and stops the running scdaemon. Prints each change.
    ///
    /// Nothing is written when the line is already present. An existing
    /// backup is kept as it is, never overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`GpgError`] when another `scdaemon-program` is set, a file
    /// cannot be read or written, or gpgconf fails.
    #[tracing::instrument(skip_all)]
    pub async fn install() -> Result<(), GpgError> {
        let program = current_program()?;
        if looks_versioned(Path::new(&program)) {
            tracing::warn!(
                path = %program,
                "touchcue runs from a versioned directory that an upgrade may remove; \
                 after upgrading, run `touchcue gpg uninstall` and `touchcue gpg install` again"
            );
        }
        let home = gpgconf_dir(None, "homedir").await?;
        let conf = home.join(CONF_FILE);
        if let Some(text) = read(&conf)? {
            let Some(edited) = add_program(&text, &program)? else {
                println!("{} already sets {OPTION} {program}", conf.display());
                return Ok(());
            };
            let backup = home.join(BACKUP_FILE);
            match create(&backup, &text) {
                Ok(()) => println!("backed up {} to {}", conf.display(), backup.display()),
                Err(GpgError::File { source, .. }) if source.kind() == ErrorKind::AlreadyExists => {
                    println!("kept the existing backup {}", backup.display());
                }
                Err(error) => return Err(error),
            }
            write(&conf, &edited)?;
        } else {
            let Some(created) = add_program("", &program)? else {
                return Ok(());
            };
            create(&conf, &created)?;
            println!("created {}", conf.display());
        }
        println!("added `{OPTION} {program}` to {}", conf.display());
        apply().await
    }

    /// Removes the `scdaemon-program` line that names touchcue from
    /// `gpg-agent.conf`, or with `restore_backup` replaces the file with the
    /// backup taken by [`install`] and deletes the backup. Then reloads
    /// gpg-agent and stops the running scdaemon. Prints each change.
    ///
    /// Restoring is refused when the file, without the touchcue line, differs
    /// from the backup, since that would discard later changes.
    ///
    /// # Errors
    ///
    /// Returns [`GpgError`] when `scdaemon-program` names another program,
    /// the backup is missing or differs for `restore_backup`, a file cannot
    /// be read or written, or gpgconf fails.
    #[tracing::instrument(skip_all, fields(restore_backup = restore_backup))]
    pub async fn uninstall(restore_backup: bool) -> Result<(), GpgError> {
        let program = current_program()?;
        let home = gpgconf_dir(None, "homedir").await?;
        let conf = home.join(CONF_FILE);
        let current = read(&conf)?;
        let edited = match &current {
            Some(text) => remove_program(text, &program)?,
            None => None,
        };
        if restore_backup {
            let backup = home.join(BACKUP_FILE);
            let Some(saved) = read(&backup)? else {
                return Err(GpgError::NoBackup { path: backup });
            };
            let unchanged = without_program(current.as_deref().unwrap_or_default(), &program)?;
            if unchanged != saved {
                return Err(GpgError::RestoreDiscards { conf, backup });
            }
            write(&conf, &saved)?;
            fs::remove_file(&backup).map_err(|source| GpgError::File {
                action: "remove",
                path: backup.clone(),
                source,
            })?;
            println!(
                "restored {} from {} and removed the backup",
                conf.display(),
                backup.display()
            );
        } else if let Some(edited) = edited {
            write(&conf, &edited)?;
            println!("removed {OPTION} from {}", conf.display());
        } else {
            println!("{} does not set {OPTION} to touchcue", conf.display());
            return Ok(());
        }
        apply().await
    }

    /// Reloads gpg-agent's configuration and stops scdaemon, so that the
    /// next card access starts the configured program.
    async fn apply() -> Result<(), GpgError> {
        gpgconf(&[OsStr::new("--reload"), OsStr::new("gpg-agent")]).await?;
        println!("reloaded gpg-agent");
        gpgconf(&[OsStr::new("--kill"), OsStr::new("scdaemon")]).await?;
        println!("stopped scdaemon; gpg-agent starts it again on the next card access");
        Ok(())
    }

    /// Returns the canonical path of the running executable as text.
    fn current_program() -> Result<String, GpgError> {
        let path = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(GpgError::CurrentExe)?;
        path.into_os_string()
            .into_string()
            .map_err(|path| GpgError::NonUtf8Path {
                path: PathBuf::from(path),
            })
    }

    /// Reads `path` as text, or returns `None` when it does not exist.
    fn read(path: &Path) -> Result<Option<String>, GpgError> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(GpgError::File {
                action: "read",
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Replaces the file at `path` with `text` atomically: writes a
    /// temporary file in the same directory, syncs it and renames it over
    /// the target. A symlink is resolved first and kept, and the target's
    /// permissions are kept. A missing file is created as [`create`] does.
    fn write(path: &Path, text: &str) -> Result<(), GpgError> {
        let target = match fs::canonicalize(path) {
            Ok(target) => target,
            Err(error) if error.kind() == ErrorKind::NotFound => return create(path, text),
            Err(source) => return Err(file_error("resolve", path)(source)),
        };
        let permissions = fs::metadata(&target)
            .map_err(file_error("inspect", &target))?
            .permissions();
        let temp = write_temp(&target, text)?;
        let replaced = fs::set_permissions(&temp, permissions)
            .map_err(file_error("set the mode of", &temp))
            .and_then(|()| fs::rename(&temp, &target).map_err(file_error("replace", &target)));
        if let Err(error) = replaced {
            remove_temp(&temp);
            return Err(error);
        }
        sync_parent(&target);
        Ok(())
    }

    /// Creates `path` with `text` and mode 0600; fails when it already
    /// exists. The contents are written to a temporary file first and then
    /// linked into place, so `path` never holds a partial file.
    fn create(path: &Path, text: &str) -> Result<(), GpgError> {
        let temp = write_temp(path, text)?;
        let linked = fs::hard_link(&temp, path).map_err(file_error("create", path));
        remove_temp(&temp);
        linked?;
        sync_parent(path);
        Ok(())
    }

    /// Writes `text` to a new, synced temporary file with mode 0600 next to
    /// `path` and returns its path. A partly written file is removed.
    fn write_temp(path: &Path, text: &str) -> Result<PathBuf, GpgError> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(GpgError::NotAFile {
                path: path.to_owned(),
            });
        };
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".touchcue-{}.tmp", std::process::id()));
        let temp = dir.join(temp_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temp)
            .map_err(file_error("create", &temp))?;
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(file_error("write", &temp));
        if let Err(error) = written {
            drop(file);
            remove_temp(&temp);
            return Err(error);
        }
        Ok(temp)
    }

    /// Removes a temporary file, recording a failure.
    fn remove_temp(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                error = &error as &dyn std::error::Error,
                path = %path.display(),
                "cannot remove a temporary file"
            ),
        }
    }

    /// Syncs the directory of `path`, which makes a rename or link in it
    /// durable; a failure is recorded, since the change itself is done.
    fn sync_parent(path: &Path) {
        let Some(dir) = path.parent() else {
            return;
        };
        if let Err(error) = File::open(dir).and_then(|dir| dir.sync_all()) {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                path = %dir.display(),
                "cannot sync the directory"
            );
        }
    }

    fn file_error(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> GpgError {
        let path = path.to_owned();
        move |source| GpgError::File {
            action,
            path,
            source,
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        #[derive(Debug, thiserror::Error)]
        enum TestError {
            #[error(transparent)]
            Io(#[from] std::io::Error),
            #[error(transparent)]
            Gpg(#[from] GpgError),
        }

        fn entries(dir: &Path) -> Result<Vec<OsString>, TestError> {
            let mut names = Vec::new();
            for entry in fs::read_dir(dir)? {
                names.push(entry?.file_name());
            }
            names.sort();
            Ok(names)
        }

        #[test]
        fn create_never_replaces_and_leaves_no_temporary_file() -> Result<(), TestError> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("gpg-agent.conf.touchcue-backup");
            create(&path, "first\n")?;
            assert_eq!(fs::read_to_string(&path)?, "first\n");
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, FILE_MODE);
            assert!(matches!(
                create(&path, "second\n"),
                Err(GpgError::File { source, .. }) if source.kind() == ErrorKind::AlreadyExists
            ));
            assert_eq!(fs::read_to_string(&path)?, "first\n");
            assert_eq!(
                entries(dir.path())?,
                [OsString::from("gpg-agent.conf.touchcue-backup")]
            );
            Ok(())
        }

        #[test]
        fn write_replaces_and_leaves_no_temporary_file() -> Result<(), TestError> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("gpg-agent.conf");
            fs::write(&path, "old\n")?;
            write(&path, "new\n")?;
            assert_eq!(fs::read_to_string(&path)?, "new\n");
            assert_eq!(entries(dir.path())?, [OsString::from("gpg-agent.conf")]);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = "/usr/bin/touchcue";

    fn foreign(program: &str) -> EditError {
        EditError::Foreign {
            program: program.to_owned(),
        }
    }

    #[test]
    fn install_appends_one_line() {
        assert_eq!(
            add_program("default-cache-ttl 600\n", PROGRAM),
            Ok(Some(format!(
                "default-cache-ttl 600\nscdaemon-program {PROGRAM}\n"
            )))
        );
        assert_eq!(
            add_program("# no newline", PROGRAM),
            Ok(Some(format!("# no newline\nscdaemon-program {PROGRAM}\n")))
        );
        assert_eq!(
            add_program("", PROGRAM),
            Ok(Some(format!("scdaemon-program {PROGRAM}\n")))
        );
    }

    #[test]
    fn install_refuses_a_program_with_control_characters() {
        for program in ["/opt/touch\ncue/touchcue", "/opt/touchcue\r", "/opt/t\u{7}"] {
            assert_eq!(
                add_program("", program),
                Err(EditError::ControlCharacter {
                    program: program.to_owned()
                })
            );
        }
    }

    #[test]
    fn install_is_a_no_op_when_present() {
        let conf = format!("  scdaemon-program  \"{PROGRAM}\"  \n");
        assert_eq!(add_program(&conf, PROGRAM), Ok(None));
    }

    #[test]
    fn install_refuses_a_foreign_program() {
        let conf = "scdaemon-program /usr/libexec/scdaemon\n";
        assert_eq!(
            add_program(conf, PROGRAM),
            Err(foreign("/usr/libexec/scdaemon"))
        );
    }

    #[test]
    fn comments_and_lookalikes_are_not_options() {
        let conf = "# scdaemon-program /x\nscdaemon-programs /y\nscdaemon-program\n";
        assert_eq!(scdaemon_programs(conf), [""]);
        assert_eq!(
            scdaemon_programs("#scdaemon-program /x\n"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn uninstall_removes_only_touchcue_lines() {
        let conf = format!(
            "default-cache-ttl 600\r\nscdaemon-program {PROGRAM}\n\
             scdaemon-program /old/path/touchcue\nmax-cache-ttl 7200"
        );
        assert_eq!(
            remove_program(&conf, PROGRAM),
            Ok(Some(
                "default-cache-ttl 600\r\nmax-cache-ttl 7200".to_owned()
            ))
        );
    }

    #[test]
    fn uninstall_without_a_line_changes_nothing() {
        assert_eq!(remove_program("max-cache-ttl 7200\n", PROGRAM), Ok(None));
    }

    #[test]
    fn uninstall_refuses_a_foreign_program() {
        let conf = format!("scdaemon-program {PROGRAM}\nscdaemon-program /usr/libexec/scdaemon\n");
        assert_eq!(
            remove_program(&conf, PROGRAM),
            Err(foreign("/usr/libexec/scdaemon"))
        );
    }

    #[test]
    fn versioned_directories_are_recognized() {
        for path in [
            "/home/u/.local/share/mise/installs/touchcue/latest/bin/touchcue",
            "/home/u/.cargo/registry/bin/touchcue",
            "/opt/touchcue-0.1.0/touchcue",
            "/opt/v1.2/bin/touchcue",
        ] {
            assert!(looks_versioned(Path::new(path)), "{path}");
        }
        for path in [
            "/usr/bin/touchcue",
            "/home/u/.cargo/bin/touchcue",
            "/home/u/.local/bin/touchcue",
            "/opt/touchcue2/touchcue1.0",
        ] {
            assert!(!looks_versioned(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn without_program_keeps_other_lines() {
        assert_eq!(
            without_program(&format!("a\nscdaemon-program {PROGRAM}\nb\n"), PROGRAM),
            Ok("a\nb\n".to_owned())
        );
        assert_eq!(without_program("a\n", PROGRAM), Ok("a\n".to_owned()));
    }

    #[test]
    fn gpgconf_escapes_are_decoded() {
        assert_eq!(percent_decode("/home/a%3ab/%25x"), b"/home/a:b/%x");
        assert_eq!(percent_decode("/bad%zz%4"), b"/bad%zz%4");
    }
}
