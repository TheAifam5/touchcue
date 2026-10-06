//! The scdaemon wrapper and `touchcue gpg` as real processes, against a fake
//! `gpgconf` and a fake scdaemon written as shell scripts.
#![cfg(target_os = "linux")]

use std::fs::{self, DirBuilder, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use touchcue_core::helper::{Message, Origin, ParseError};
use touchcue_core::{Op, Outcome};

/// Longest wait for a process or socket event.
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

/// Answers each command line; `PKSIGN` stays silent for longer than the
/// show delay, `WAIT` blocks until SIGTERM, which exits with 42. Exits with
/// 3 at EOF.
const FAKE_SCDAEMON: &str = r#"#!/bin/sh
trap 'kill "$sleeper" 2>/dev/null; exit 42' TERM
printf '# args: %s\n' "$*"
printf 'OK Pleased to meet you\n'
while IFS= read -r line; do
  case "$line" in
    PKSIGN*) sleep 1; printf 'D %%01%%02\nOK\n' ;;
    WAIT) sleep 30 & sleeper=$!; wait "$sleeper" ;;
    *) printf 'S PROGRESS \001\377\nOK\n' ;;
  esac
done
exit 3
"#;

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Errno(#[from] rustix::io::Errno),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Pid(#[from] std::num::TryFromIntError),
    #[error("{0}")]
    Failed(String),
}

fn failed(message: impl Into<String>) -> TestError {
    TestError::Failed(message.into())
}

/// Temporary `PATH`, gnupg home and runtime directories.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Result<Self, TestError> {
        let root = tempfile::tempdir()?;
        let env = Self { root };
        for dir in ["bin", "libexec", "gnupg"] {
            fs::create_dir(env.path(dir))?;
        }
        for dir in ["run/touchcue", "sock"] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(env.path(dir))?;
        }
        let gpgconf = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = --homedir ]; then shift 2; fi\n\
             case \"$1 $2\" in\n\
             '--list-dirs socketdir') printf '%s\\n' '{sock}' ;;\n\
             '--list-dirs homedir') printf '%s\\n' '{home}' ;;\n\
             '--list-dirs libexecdir') printf '%s\\n' '{libexec}' ;;\n\
             '--reload gpg-agent'|'--kill scdaemon') ;;\n\
             *) exit 1 ;;\n\
             esac\n",
            log = env.path("gpgconf.log").display(),
            home = env.path("gnupg").display(),
            libexec = env.path("libexec").display(),
            sock = env.path("sock").display(),
        );
        write_script(&env.path("bin/gpgconf"), &gpgconf)?;
        Ok(env)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn command(&self) -> Result<Command, TestError> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.path("bin")];
        paths.extend(std::env::split_paths(&path));
        let mut command = Command::new(env!("CARGO_BIN_EXE_touchcue"));
        command
            .env(
                "PATH",
                std::env::join_paths(paths).map_err(|e| failed(e.to_string()))?,
            )
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("FAKE_SCDAEMON_SOCKET", self.path("sock/S.scdaemon"))
            .env_remove("TOUCHCUE_LOG");
        Ok(command)
    }

    fn spawn_wrapper(&self) -> Result<Child, TestError> {
        Ok(self
            .command()?
            .args(["--multi-server", "--homedir", "/h"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    }

    fn gpg(&self, args: &[&str]) -> Result<Output, TestError> {
        Ok(self
            .command()?
            .arg("gpg")
            .args(args)
            .stdin(Stdio::null())
            .output()?)
    }

    fn gpgconf_log(&self) -> Result<String, TestError> {
        match fs::read_to_string(self.path("gpgconf.log")) {
            Ok(log) => Ok(log),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }
}

fn write_script(path: &Path, text: &str) -> Result<(), TestError> {
    fs::write(path, text)?;
    fs::set_permissions(path, Permissions::from_mode(0o700))?;
    Ok(())
}

fn wait(child: &mut Child) -> Result<ExitStatus, TestError> {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(POLL);
    }
    child.kill()?;
    child.wait()?;
    Err(failed("process did not exit"))
}

fn accept(listener: &UnixListener) -> Result<UnixStream, TestError> {
    listener.set_nonblocking(true)?;
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) => return Err(error.into()),
        }
    }
    Err(failed("wrapper did not connect to the helper socket"))
}

fn read_message(reader: &mut BufReader<UnixStream>) -> Result<Message, TestError> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(Message::parse(&line)?)
}

/// Reads `count` lines, bytes unchanged.
fn read_lines(reader: &mut impl BufRead, count: usize) -> Result<Vec<u8>, TestError> {
    let mut bytes = Vec::new();
    for _ in 0..count {
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            return Err(failed("unexpected EOF"));
        }
    }
    Ok(bytes)
}

fn stderr_of(child: &mut Child) -> Result<String, TestError> {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut text)?;
    }
    Ok(text)
}

#[test]
fn wrapper_forwards_bytes_and_reports_a_slow_sign() -> Result<(), TestError> {
    let env = Env::new()?;
    write_script(&env.path("libexec/scdaemon"), FAKE_SCDAEMON)?;
    let listener = UnixListener::bind(env.path("run/touchcue/helper.sock"))?;
    let mut child = env.spawn_wrapper()?;

    let mut stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?);
    // Assuan clients wait for each response before the next command.
    let mut output = read_lines(&mut stdout, 2)?;
    stdin.write_all(b"GETINFO version\n")?;
    output.extend(read_lines(&mut stdout, 2)?);
    stdin.write_all(b"PKSIGN --hash=sha256 OPENPGP.1\n")?;
    drop(stdin);
    stdout.read_to_end(&mut output)?;
    let status = wait(&mut child)?;

    let expected: &[u8] = b"# args: --multi-server --homedir /h\n\
                            OK Pleased to meet you\n\
                            S PROGRESS \x01\xff\nOK\n\
                            D %01%02\nOK\n";
    assert_eq!(output, expected);
    let stderr = stderr_of(&mut child)?;
    assert_eq!(status.code(), Some(3), "stderr: {stderr}");
    // No S.scdaemon exists here, so the proxy is skipped with one warning.
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 1, "stderr: {stderr}");
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("touchcue-scdaemon: ")
                && line.contains("WARN")
                && line.contains("scdaemon socket not proxied")
                && !line.contains('\x1b')),
        "stderr: {stderr}"
    );

    let mut reader = BufReader::new(accept(&listener)?);
    let Message::Start {
        origin: Origin::Scdaemon,
        seq,
        op: Op::Sign,
        detail: None,
    } = read_message(&mut reader)?
    else {
        return Err(failed("expected a sign start"));
    };
    assert_eq!(
        read_message(&mut reader)?,
        Message::End {
            origin: Origin::Scdaemon,
            seq,
            outcome: Outcome::Touched,
        }
    );
    Ok(())
}

/// Device and inode numbers of the file at `path`.
fn inode(path: &Path) -> Result<(u64, u64), TestError> {
    let meta = fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// Installs `examples/fake_scdaemon` as the real scdaemon, listening on
/// `sock/S.scdaemon`.
fn use_fake_with_socket(env: &Env) -> Result<(), TestError> {
    let fake = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("examples/fake_scdaemon"))
        .filter(|path| path.exists())
        .ok_or_else(|| failed("examples/fake_scdaemon is not built"))?;
    std::os::unix::fs::symlink(fake, env.path("libexec/scdaemon"))?;
    Ok(())
}

/// Waits until `path` exists.
fn wait_for_path(child: &mut Child, path: &Path) -> Result<(), TestError> {
    let started = Instant::now();
    while !path.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(failed(format!("wrapper exited early: {status}")));
        }
        if started.elapsed() > TIMEOUT {
            return Err(failed(format!("{} did not appear", path.display())));
        }
        thread::sleep(POLL);
    }
    Ok(())
}

#[test]
fn wrapper_proxies_and_reports_the_scdaemon_socket() -> Result<(), TestError> {
    let env = Env::new()?;
    use_fake_with_socket(&env)?;
    let helper = UnixListener::bind(env.path("run/touchcue/helper.sock"))?;
    let public = env.path("sock/S.scdaemon");
    let moved = env.path("sock/S.scdaemon.touchcue");
    let mut child = env.spawn_wrapper()?;
    let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?);
    assert_eq!(read_lines(&mut stdout, 1)?, b"OK Pleased to meet you\n");

    wait_for_path(&mut child, &moved)?;
    assert_ne!(inode(&public)?, inode(&moved)?);
    assert_eq!(fs::symlink_metadata(&public)?.mode() & 0o777, 0o600);

    let mut client = UnixStream::connect(&public)?;
    client.set_read_timeout(Some(TIMEOUT))?;
    client.write_all(b"PKSIGN OPENPGP.1\n")?;
    let mut client = BufReader::new(client);
    assert_eq!(read_lines(&mut client, 2)?, b"D \x01\xff\nOK\n");

    let mut reports = BufReader::new(accept(&helper)?);
    let Message::Start {
        origin: Origin::Scdaemon,
        seq,
        op: Op::Sign,
        detail: None,
    } = read_message(&mut reports)?
    else {
        return Err(failed("expected a sign start"));
    };
    assert_eq!(
        read_message(&mut reports)?,
        Message::End {
            origin: Origin::Scdaemon,
            seq,
            outcome: Outcome::Touched,
        }
    );

    drop(stdin);
    let status = wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;
    assert_eq!(status.code(), Some(3), "stderr: {stderr}");
    assert!(!public.exists(), "proxy socket left behind");
    assert!(!moved.exists(), "moved scdaemon socket left behind");
    assert_eq!(stderr, "", "a proxied socket logs nothing");
    Ok(())
}

#[test]
fn wrapper_leaves_a_foreign_socket_alone() -> Result<(), TestError> {
    let env = Env::new()?;
    write_script(&env.path("libexec/scdaemon"), FAKE_SCDAEMON)?;
    // Served by this test, not by the wrapper's child.
    let public = env.path("sock/S.scdaemon");
    let _foreign = UnixListener::bind(&public)?;
    let before = inode(&public)?;
    let mut child = env.spawn_wrapper()?;
    let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?);
    read_lines(&mut stdout, 2)?;
    drop(stdin);
    wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;
    assert_eq!(inode(&public)?, before);
    assert!(!env.path("sock/S.scdaemon.touchcue").exists());
    assert!(
        stderr.contains("not served by the scdaemon this wrapper started"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn wrapper_refuses_a_shared_socket_directory() -> Result<(), TestError> {
    let env = Env::new()?;
    use_fake_with_socket(&env)?;
    fs::set_permissions(env.path("sock"), Permissions::from_mode(0o777))?;
    let mut child = env.spawn_wrapper()?;
    let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?);
    read_lines(&mut stdout, 1)?;
    drop(stdin);
    wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;
    assert!(!env.path("sock/S.scdaemon.touchcue").exists());
    assert!(
        stderr.contains("is not a directory private to this user"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn wrapper_works_without_a_daemon_and_forwards_sigterm() -> Result<(), TestError> {
    let env = Env::new()?;
    write_script(&env.path("libexec/scdaemon"), FAKE_SCDAEMON)?;
    let mut child = env.spawn_wrapper()?;
    let mut stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?);
    read_lines(&mut stdout, 2)?;
    stdin.write_all(b"PKSIGN OPENPGP.1\n")?;
    assert_eq!(read_lines(&mut stdout, 2)?, b"D %01%02\nOK\n");
    stdin.write_all(b"WAIT\n")?;

    // Gives sh time to read WAIT and start waiting.
    thread::sleep(Duration::from_millis(200));
    let pid = Pid::from_raw(i32::try_from(child.id())?).ok_or_else(|| failed("invalid pid"))?;
    // Non-dumpable: other processes of the user cannot read its descriptors
    // or memory. Root can, so the check needs an unprivileged run.
    if !rustix::process::geteuid().is_root() {
        match fs::read_dir(format!("/proc/{}/fd", child.id())) {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
            other => return Err(failed(format!("wrapper is dumpable: {other:?}"))),
        }
    }
    // Attribution still reads its parent and start time.
    fs::read_to_string(format!("/proc/{}/stat", child.id()))?;
    kill_process(pid, Signal::TERM)?;
    let status = wait(&mut child)?;
    drop(stdin);
    assert_eq!(
        status.code(),
        Some(42),
        "stderr: {}",
        stderr_of(&mut child)?
    );
    Ok(())
}

#[test]
fn wrapper_refuses_to_start_itself() -> Result<(), TestError> {
    let env = Env::new()?;
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_touchcue"), env.path("libexec/scdaemon"))?;
    let mut child = env.spawn_wrapper()?;
    let status = wait(&mut child)?;
    assert!(!status.success());
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(|| failed("no stdout"))?
        .read_to_end(&mut output)?;
    assert_eq!(output, b"");
    Ok(())
}

#[test]
fn gpg_install_and_uninstall_edit_one_line() -> Result<(), TestError> {
    let env = Env::new()?;
    let conf = env.path("gnupg/gpg-agent.conf");
    // A dotfile manager's link; edits go to its target and keep the link.
    let real = env.path("gnupg/managed.conf");
    let backup = env.path("gnupg/gpg-agent.conf.touchcue-backup");
    let original = "default-cache-ttl 600\n";
    fs::write(&real, original)?;
    fs::set_permissions(&real, Permissions::from_mode(0o640))?;
    std::os::unix::fs::symlink("managed.conf", &conf)?;
    let program = fs::canonicalize(env!("CARGO_BIN_EXE_touchcue"))?;
    let line = format!("scdaemon-program {}\n", program.display());
    let succeeds = |args: &[&str]| -> Result<(), TestError> {
        let output = env.gpg(args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failed(format!("{args:?}: {output:?}")))
        }
    };

    succeeds(&["install"])?;
    assert_eq!(fs::read_to_string(&conf)?, format!("{original}{line}"));
    assert!(fs::symlink_metadata(&conf)?.file_type().is_symlink());
    assert_eq!(fs::metadata(&real)?.mode() & 0o777, 0o640);
    assert_eq!(fs::read_to_string(&backup)?, original);
    assert_eq!(
        env.gpgconf_log()?,
        "--list-dirs homedir\n--reload gpg-agent\n--kill scdaemon\n"
    );

    succeeds(&["install"])?;
    assert_eq!(fs::read_to_string(&conf)?, format!("{original}{line}"));

    succeeds(&["uninstall", "--restore-backup"])?;
    assert_eq!(fs::read_to_string(&conf)?, original);
    assert!(fs::symlink_metadata(&conf)?.file_type().is_symlink());
    assert!(!backup.exists());

    succeeds(&["install"])?;
    succeeds(&["uninstall"])?;
    assert_eq!(fs::read_to_string(&conf)?, original);
    assert!(backup.exists());

    // Changes made after install survive: restoring is refused.
    let edited = format!("{original}max-cache-ttl 7200\n");
    fs::write(&conf, &edited)?;
    succeeds(&["install"])?;
    let output = env.gpg(&["uninstall", "--restore-backup"])?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("was changed after install"));
    assert_eq!(fs::read_to_string(&conf)?, format!("{edited}{line}"));
    assert_eq!(fs::read_to_string(&backup)?, original);
    Ok(())
}

#[test]
fn gpg_install_refuses_a_foreign_program_and_keeps_a_backup() -> Result<(), TestError> {
    let env = Env::new()?;
    let conf = env.path("gnupg/gpg-agent.conf");
    let backup = env.path("gnupg/gpg-agent.conf.touchcue-backup");

    let foreign = "scdaemon-program /usr/libexec/scdaemon\n";
    fs::write(&conf, foreign)?;
    let output = env.gpg(&["install"])?;
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&conf)?, foreign);
    assert!(!backup.exists());
    assert!(!env.gpgconf_log()?.contains("--reload"));

    fs::write(&conf, "max-cache-ttl 7200\n")?;
    fs::write(&backup, "old\n")?;
    let output = env.gpg(&["install"])?;
    assert!(output.status.success(), "{output:?}");
    let program = fs::canonicalize(env!("CARGO_BIN_EXE_touchcue"))?;
    assert_eq!(
        fs::read_to_string(&conf)?,
        format!(
            "max-cache-ttl 7200\nscdaemon-program {}\n",
            program.display()
        )
    );
    assert_eq!(fs::read_to_string(&backup)?, "old\n");
    assert!(String::from_utf8_lossy(&output.stdout).contains("kept the existing backup"));
    assert!(env.gpgconf_log()?.contains("--reload gpg-agent"));
    Ok(())
}
