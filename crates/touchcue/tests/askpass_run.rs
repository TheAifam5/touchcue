//! `touchcue askpass` entry point with real signals and a real fallback exec.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::io::{BufRead as _, BufReader, ErrorKind};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Signal, getpid, kill_process};
use touchcue::askpass::{AskpassError, Environment, FALLBACK_ENV, run_with};
use touchcue_core::helper::{Message, Origin, ParseError};
use touchcue_core::{Op, Outcome};

/// Longest wait for askpass to connect or write a line.
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
/// Set in the re-executed test binary that runs the fallback path.
const CHILD_ENV: &str = "TOUCHCUE_ASKPASS_TEST_CHILD";

const NOTICE: &str = "Confirm user presence for key ED25519-SK SHA256:abc\n\
                      public key authentication request for user \"git\" to listed host";

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Errno(#[from] rustix::io::Errno),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Askpass(#[from] AskpassError),
    #[error("{0}")]
    Failed(String),
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
    Err(TestError::Failed("askpass did not connect".to_owned()))
}

fn read_message(reader: &mut BufReader<UnixStream>) -> Result<Message, TestError> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(TestError::Failed("connection closed".to_owned()));
    }
    Ok(Message::parse(&line)?)
}

#[test]
fn notice_reports_start_and_end_on_sigterm() -> Result<(), TestError> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join("touchcue"))?;
    let listener = UnixListener::bind(dir.path().join("touchcue/helper.sock"))?;
    let env = Environment {
        prompt: Some("none".into()),
        runtime_dir: Some(dir.path().to_owned()),
        fallback: None,
    };
    let askpass = thread::spawn(move || run_with(&[OsString::from(NOTICE)], &env));

    let mut reader = BufReader::new(accept(&listener)?);
    assert_eq!(
        read_message(&mut reader)?,
        Message::Start {
            origin: Origin::Askpass,
            seq: 1,
            op: Op::Auth,
            detail: Some("ED25519-SK SHA256:abc → user git".to_owned()),
        }
    );
    // askpass registers its handlers before it connects, so this is caught.
    kill_process(getpid(), Signal::TERM)?;
    assert_eq!(
        read_message(&mut reader)?,
        Message::End {
            origin: Origin::Askpass,
            seq: 1,
            outcome: Outcome::Touched,
        }
    );
    let code = match askpass.join() {
        Ok(result) => result?,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert_eq!(code, ExitCode::SUCCESS);
    Ok(())
}

/// Writes an askpass stand-in that prints its arguments and environment
/// markers and exits with status 3.
fn fake_fallback(dir: &Path) -> Result<std::path::PathBuf, TestError> {
    let path = dir.join("fallback");
    fs::write(
        &path,
        "#!/bin/sh\n\
         printf 'arg=%s\\n' \"$@\"\n\
         printf 'prompt=%s\\n' \"$SSH_ASKPASS_PROMPT\"\n\
         printf 'fallback=%s\\n' \"${TOUCHCUE_ASKPASS_FALLBACK-unset}\"\n\
         exit 3\n",
    )?;
    fs::set_permissions(&path, Permissions::from_mode(0o700))?;
    Ok(path)
}

#[test]
fn confirm_prompt_execs_the_fallback() -> Result<(), TestError> {
    if std::env::var_os(CHILD_ENV).is_some() {
        // Returns only if the exec failed.
        let result = run_with(
            &[OsString::from("Allow use of key k? ")],
            &Environment::from_process(),
        );
        return Err(TestError::Failed(format!("exec returned {result:?}")));
    }
    let dir = tempfile::tempdir()?;
    let fallback = fake_fallback(dir.path())?;
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "confirm_prompt_execs_the_fallback",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env(FALLBACK_ENV, &fallback)
        .env("SSH_ASKPASS_PROMPT", "confirm")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    for expected in [
        "arg=Allow use of key k? \n",
        "prompt=confirm\n",
        "fallback=unset\n",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in\n{stdout}"
        );
    }
    Ok(())
}
