//! The pinentry wrapper as a real process, against a fake pinentry written
//! as a shell script and a fake daemon helper socket.
#![cfg(target_os = "linux")]

use std::fs::{self, DirBuilder, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use touchcue_core::Outcome;
use touchcue_core::helper::{Message, Origin, ParseError};

/// Longest wait for a process or socket event.
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);
/// Placed in every description and PIN this test sends.
const SENTINEL: &str = "SENTINEL-7f3a";

/// Logs every line it reads to `$FAKE_PINENTRY_LOG` and answers it.
/// `GETPIN` inquires the quality, reads the reply up to `END`, then answers
/// with the bytes of `$FAKE_PINENTRY_DATA`; `GETPIN hang` blocks until
/// SIGTERM, which exits with 42; `CONFIRM` is cancelled. Exits with 3 at
/// EOF.
const FAKE_PINENTRY: &str = r#"#!/bin/sh
trap 'kill "$sleeper" 2>/dev/null; exit 42' TERM
printf '# args: %s\n' "$*"
printf 'OK Pleased to meet you\n'
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$FAKE_PINENTRY_LOG"
  case "$line" in
    'GETPIN hang') /bin/sleep 30 & sleeper=$!; wait "$sleeper" ;;
    GETPIN)
      printf 'INQUIRE QUALITY\n'
      while IFS= read -r reply; do
        printf '%s\n' "$reply" >> "$FAKE_PINENTRY_LOG"
        [ "$reply" = END ] && break
      done
      /bin/cat "$FAKE_PINENTRY_DATA"
      printf 'OK\n' ;;
    CONFIRM*) printf 'ERR 83886179 Operation cancelled <Pinentry>\n' ;;
    *) printf 'OK\n' ;;
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

/// Temporary `PATH` and runtime directories with the fake pinentry.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Result<Self, TestError> {
        let root = tempfile::tempdir()?;
        let env = Self { root };
        fs::create_dir(env.path("bin"))?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(env.path("run/touchcue"))?;
        let script = env.path("bin/pinentry");
        fs::write(&script, FAKE_PINENTRY)?;
        fs::set_permissions(&script, Permissions::from_mode(0o700))?;
        Ok(env)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn listen(&self) -> Result<UnixListener, TestError> {
        Ok(UnixListener::bind(self.path("run/touchcue/helper.sock"))?)
    }

    /// The wrapper started as `program`, with the fake pinentry first on
    /// `PATH` and no `TOUCHCUE_PINENTRY`.
    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env("PATH", self.path("bin"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("FAKE_PINENTRY_LOG", self.path("received"))
            .env("FAKE_PINENTRY_DATA", self.path("data"))
            .env_remove("TOUCHCUE_PINENTRY")
            .env_remove("TOUCHCUE_PINENTRY_ACTIVE")
            .env_remove("TOUCHCUE_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `touchcue pinentry` with `args`.
    fn subcommand(&self, args: &[&str]) -> Command {
        let mut command = self.command(Path::new(env!("CARGO_BIN_EXE_touchcue")));
        command.arg("pinentry").args(args);
        command
    }

    /// A link to touchcue named `touchcue-pinentry`, started with `args`.
    fn link(&self, args: &[&str]) -> Result<Command, TestError> {
        let link = self.path("touchcue-pinentry");
        symlink(env!("CARGO_BIN_EXE_touchcue"), &link)?;
        let mut command = self.command(&link);
        command.args(args);
        Ok(command)
    }

    fn received(&self) -> Result<Vec<u8>, TestError> {
        Ok(fs::read(self.path("received"))?)
    }
}

/// gpg-agent's side of the wrapper's stdio.
struct Agent {
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Every byte read from the wrapper.
    output: Vec<u8>,
    /// Every byte written to the wrapper.
    input: Vec<u8>,
}

impl Agent {
    fn new(child: &mut Child) -> Result<Self, TestError> {
        Ok(Self {
            stdin: Some(child.stdin.take().ok_or_else(|| failed("no stdin"))?),
            stdout: BufReader::new(child.stdout.take().ok_or_else(|| failed("no stdout"))?),
            output: Vec::new(),
            input: Vec::new(),
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), TestError> {
        self.stdin
            .as_mut()
            .ok_or_else(|| failed("stdin closed"))?
            .write_all(bytes)?;
        self.input.extend_from_slice(bytes);
        Ok(())
    }

    /// Reads one line and returns it, bytes unchanged.
    fn line(&mut self) -> Result<Vec<u8>, TestError> {
        let mut line = Vec::new();
        if self.stdout.read_until(b'\n', &mut line)? == 0 {
            return Err(failed("unexpected EOF"));
        }
        self.output.extend_from_slice(&line);
        Ok(line)
    }

    fn exact(&mut self, len: usize) -> Result<Vec<u8>, TestError> {
        let mut bytes = vec![0; len];
        self.stdout.read_exact(&mut bytes)?;
        self.output.extend_from_slice(&bytes);
        Ok(bytes)
    }

    fn close(&mut self) {
        self.stdin = None;
    }

    fn rest(&mut self) -> Result<(), TestError> {
        self.stdout.read_to_end(&mut self.output)?;
        Ok(())
    }
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

fn stderr_of(child: &mut Child) -> Result<String, TestError> {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut text)?;
    }
    Ok(text)
}

/// Reads every report the wrapper sent; call after it exited, which is
/// after its reports were delivered.
fn reports(listener: &UnixListener) -> Result<Vec<Message>, TestError> {
    listener.set_nonblocking(true)?;
    let stream = match listener.accept() {
        Ok((stream, _)) => stream,
        Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    let mut messages = Vec::new();
    for line in BufReader::new(stream).lines() {
        messages.push(Message::parse(&line?)?);
    }
    Ok(messages)
}

/// A large data line with every byte value except the line feed.
fn data_line() -> Vec<u8> {
    let mut line = b"D ".to_vec();
    line.extend(SENTINEL.bytes());
    let bytes: Vec<u8> = (0..=u8::MAX).filter(|&byte| byte != b'\n').collect();
    line.extend(bytes.iter().cycle().take(70_000));
    line.push(b'\n');
    line
}

fn owner(seq: u32) -> Message {
    Message::Owner {
        seq,
        pid: 4242,
        uid: 1000,
    }
}

fn end(seq: u32, outcome: Outcome) -> Message {
    Message::End {
        origin: Origin::Pinentry,
        seq,
        outcome,
    }
}

#[test]
fn link_forwards_bytes_and_reports_dialogs() -> Result<(), TestError> {
    let env = Env::new()?;
    let data = data_line();
    fs::write(env.path("data"), &data)?;
    let listener = env.listen()?;
    let mut child = env
        .link(&["--display", ":0", "--help"])?
        .env("TOUCHCUE_LOG", "trace")
        .spawn()?;
    let mut agent = Agent::new(&mut child)?;

    assert_eq!(agent.line()?, b"# args: --display :0 --help\n");
    assert_eq!(agent.line()?, b"OK Pleased to meet you\n");
    agent.send(b"OPTION owner=4242/1000 host.example\n")?;
    assert_eq!(agent.line()?, b"OK\n");
    agent.send(format!("SETDESC {SENTINEL}%0Afor key é\n").as_bytes())?;
    assert_eq!(agent.line()?, b"OK\n");
    agent.send(b"GETPIN\n")?;
    assert_eq!(agent.line()?, b"INQUIRE QUALITY\n");
    agent.send(format!("D {SENTINEL}\nEND\n").as_bytes())?;
    assert_eq!(agent.exact(data.len())?, data);
    assert_eq!(agent.line()?, b"OK\n");
    agent.send(b"CONFIRM\n")?;
    assert_eq!(
        agent.line()?,
        b"ERR 83886179 Operation cancelled <Pinentry>\n"
    );
    agent.close();
    agent.rest()?;
    let status = wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;

    assert_eq!(status.code(), Some(3), "stderr: {stderr}");
    assert_eq!(env.received()?, agent.input);
    let mut expected = b"# args: --display :0 --help\nOK Pleased to meet you\nOK\nOK\n\
                         INQUIRE QUALITY\n"
        .to_vec();
    expected.extend(&data);
    expected.extend(b"OK\nERR 83886179 Operation cancelled <Pinentry>\n");
    assert_eq!(agent.output, expected);
    assert!(!stderr.contains(SENTINEL), "stderr: {stderr}");
    assert!(!stderr.contains("host.example"), "stderr: {stderr}");
    assert!(
        stderr.contains("pinentry dialog opened"),
        "trace logging is on: {stderr}"
    );
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("touchcue-pinentry: ")),
        "stderr: {stderr}"
    );
    assert_eq!(
        reports(&listener)?,
        [
            owner(1),
            end(1, Outcome::Touched),
            owner(2),
            end(2, Outcome::Cancelled),
        ]
    );
    Ok(())
}

#[test]
fn subcommand_without_a_daemon_only_forwards() -> Result<(), TestError> {
    let env = Env::new()?;
    fs::write(env.path("data"), b"D 1234\n")?;
    let mut child = env.subcommand(&["--ttyname", "/dev/tty1"]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    agent.line()?;
    agent.line()?;
    agent.send(b"OPTION owner=4242/1000 host\n")?;
    agent.line()?;
    agent.send(b"GETPIN\n")?;
    agent.line()?;
    agent.send(b"END\n")?;
    assert_eq!(agent.line()?, b"D 1234\n");
    assert_eq!(agent.line()?, b"OK\n");
    agent.close();
    agent.rest()?;
    let status = wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;
    assert_eq!(status.code(), Some(3), "stderr: {stderr}");
    assert_eq!(stderr, "");
    assert_eq!(
        agent.output,
        b"# args: --ttyname /dev/tty1\nOK Pleased to meet you\nOK\n\
          INQUIRE QUALITY\nD 1234\nOK\n"
    );
    assert_eq!(env.received()?, agent.input);
    Ok(())
}

#[test]
fn malformed_and_overlong_owner_lines_are_not_reported() -> Result<(), TestError> {
    let env = Env::new()?;
    fs::write(env.path("data"), b"")?;
    let listener = env.listen()?;
    let mut child = env.subcommand(&[]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    agent.line()?;
    agent.line()?;
    for line in [
        &b"OPTION owner=4242\n"[..],
        b"OPTION owner=x/1000\n",
        b"OPTION owner=4242/1000x host\n",
    ] {
        agent.send(line)?;
        assert_eq!(agent.line()?, b"OK\n");
    }
    agent.send(b"CONFIRM\n")?;
    agent.line()?;
    // An overlong line stops observing; forwarding continues.
    let mut long = b"SETDESC ".to_vec();
    long.extend(std::iter::repeat_n(b'a', 2000));
    long.push(b'\n');
    agent.send(&long)?;
    assert_eq!(agent.line()?, b"OK\n");
    agent.send(b"OPTION owner=4242/1000 host\n")?;
    assert_eq!(agent.line()?, b"OK\n");
    agent.send(b"CONFIRM\n")?;
    agent.line()?;
    agent.close();
    agent.rest()?;
    let status = wait(&mut child)?;
    let stderr = stderr_of(&mut child)?;
    assert_eq!(status.code(), Some(3), "stderr: {stderr}");
    assert_eq!(env.received()?, agent.input);
    assert_eq!(reports(&listener)?, []);
    assert!(stderr.contains("overlong Assuan line"), "stderr: {stderr}");
    Ok(())
}

#[test]
fn sigterm_is_forwarded_and_ends_an_open_dialog_as_failed() -> Result<(), TestError> {
    let env = Env::new()?;
    let listener = env.listen()?;
    let mut child = env.subcommand(&[]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    agent.line()?;
    agent.line()?;
    agent.send(b"OPTION owner=4242/1000 host\n")?;
    agent.line()?;
    agent.send(b"GETPIN hang\n")?;

    // The fake pinentry remains in the open dialog.
    thread::sleep(Duration::from_millis(200));
    // Descriptor access is restricted for unprivileged processes.
    if !rustix::process::geteuid().is_root() {
        match fs::read_dir(format!("/proc/{}/fd", child.id())) {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {}
            other => return Err(failed(format!("wrapper is dumpable: {other:?}"))),
        }
    }
    let pid = Pid::from_raw(i32::try_from(child.id())?).ok_or_else(|| failed("invalid pid"))?;
    kill_process(pid, Signal::TERM)?;
    let status = wait(&mut child)?;
    agent.close();
    let stderr = stderr_of(&mut child)?;
    assert_eq!(status.code(), Some(42), "stderr: {stderr}");
    assert_eq!(reports(&listener)?, [owner(1), end(1, Outcome::Failed)]);
    Ok(())
}

#[test]
fn child_killed_by_a_signal_exits_with_128_plus_it() -> Result<(), TestError> {
    let env = Env::new()?;
    let script = env.path("bin/pinentry");
    fs::write(&script, "#!/bin/sh\nkill -KILL $$\n")?;
    let mut child = env.subcommand(&[]).spawn()?;
    let status = wait(&mut child)?;
    assert_eq!(status.code(), Some(128 + 9), "{}", stderr_of(&mut child)?);
    Ok(())
}

#[test]
fn wrapper_refuses_to_start_itself() -> Result<(), TestError> {
    let env = Env::new()?;
    let link = env.path("touchcue-pinentry");
    symlink(env!("CARGO_BIN_EXE_touchcue"), &link)?;
    for configured in [Some(link.as_os_str()), None] {
        let mut command = env.subcommand(&[]);
        if let Some(path) = configured {
            command.env("TOUCHCUE_PINENTRY", path);
        } else {
            // `pinentry` on PATH is touchcue itself.
            fs::remove_file(env.path("bin/pinentry"))?;
            symlink(env!("CARGO_BIN_EXE_touchcue"), env.path("bin/pinentry"))?;
        }
        let output = command.stdin(Stdio::null()).output()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert_eq!(output.stdout, b"");
        assert_eq!(stderr.lines().count(), 1, "stderr: {stderr}");
        assert!(
            stderr.starts_with("touchcue-pinentry: ") && stderr.contains("touchcue itself"),
            "stderr: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn wrapper_refuses_to_start_inside_itself() -> Result<(), TestError> {
    let env = Env::new()?;
    // `pinentry` on PATH is a script that starts the wrapper again.
    fs::write(
        env.path("bin/pinentry"),
        format!(
            "#!/bin/sh\nexec '{}' pinentry \"$@\"\n",
            env!("CARGO_BIN_EXE_touchcue")
        ),
    )?;
    let output = env.subcommand(&[]).stdin(Stdio::null()).output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(stderr.lines().count(), 1, "stderr: {stderr}");
    assert!(
        stderr.starts_with("touchcue-pinentry: ") && stderr.contains("TOUCHCUE_PINENTRY_ACTIVE"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn relative_configured_program_is_refused() -> Result<(), TestError> {
    let env = Env::new()?;
    let output = env
        .subcommand(&[])
        .env("TOUCHCUE_PINENTRY", "pinentry")
        .stdin(Stdio::null())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(stderr.lines().count(), 1, "stderr: {stderr}");
    assert!(stderr.contains("absolute path"), "stderr: {stderr}");
    Ok(())
}

#[test]
fn reports_stay_on_the_daemon_socket() -> Result<(), TestError> {
    let env = Env::new()?;
    fs::write(env.path("data"), format!("D {SENTINEL}\n"))?;
    let listener = env.listen()?;
    let mut child = env.subcommand(&[]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    agent.line()?;
    agent.line()?;
    agent.send(format!("OPTION owner=4242/1000 {SENTINEL}\n").as_bytes())?;
    agent.line()?;
    agent.send(format!("SETPROMPT {SENTINEL}\n").as_bytes())?;
    agent.line()?;
    agent.send(b"GETPIN\n")?;
    agent.line()?;
    agent.send(b"END\n")?;
    agent.line()?;
    agent.line()?;
    agent.close();
    agent.rest()?;
    wait(&mut child)?;
    let reports = reports(&listener)?;
    assert_eq!(reports, [owner(1), end(1, Outcome::Touched)]);
    let wire: String = reports.iter().map(Message::encode).collect();
    assert!(!wire.contains(SENTINEL), "{wire}");
    Ok(())
}

#[test]
fn configured_program_paths_cannot_inject_log_lines() -> Result<(), TestError> {
    let env = Env::new()?;
    let path = env.path("missing\ninjected");
    let output = env
        .subcommand(&[])
        .env("TOUCHCUE_PINENTRY", path)
        .stdin(Stdio::null())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(stderr.lines().count(), 1, "stderr: {stderr}");
    assert!(!stderr.contains("injected"), "stderr: {stderr}");
    Ok(())
}

#[test]
fn binary_input_and_output_are_identical() -> Result<(), TestError> {
    let env = Env::new()?;
    fs::write(env.path("bin/pinentry"), "#!/bin/sh\nexec /bin/cat\n")?;
    let mut child = env.subcommand(&["--", "--help"]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    let mut input = b"D ".to_vec();
    input.extend(0..=u8::MAX);
    input.extend(b"\nINQUIRE QUALITY\n");
    agent.send(&input)?;
    agent.close();
    agent.rest()?;
    let status = wait(&mut child)?;
    assert_eq!(status.code(), Some(0), "{}", stderr_of(&mut child)?);
    assert_eq!(agent.output, input);
    Ok(())
}

#[test]
fn stdin_eof_mid_dialog_reports_failed_and_reaps() -> Result<(), TestError> {
    let env = Env::new()?;
    fs::write(env.path("data"), b"")?;
    let listener = env.listen()?;
    let mut child = env.subcommand(&[]).spawn()?;
    let mut agent = Agent::new(&mut child)?;
    agent.line()?;
    agent.line()?;
    agent.send(b"OPTION owner=4242/1000 host\n")?;
    agent.line()?;
    agent.send(b"GETPIN\n")?;
    assert_eq!(agent.line()?, b"INQUIRE QUALITY\n");
    agent.close();
    agent.rest()?;
    let status = wait(&mut child)?;
    assert_eq!(status.code(), Some(3), "{}", stderr_of(&mut child)?);
    assert_eq!(reports(&listener)?, [owner(1), end(1, Outcome::Failed)]);
    Ok(())
}
