//! `touchcue run` as a real process: SIGTERM stops it cleanly and in time.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{ErrorKind, Read as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

/// Longest time the daemon may take to create its socket.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest time the daemon may take to exit after SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest time the daemon may take to exit after SIGTERM during startup;
/// shorter than the daemon's own session bus connect deadline.
const STARTUP_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// Output disabled, D-Bus and FIDO detection off, so the test touches
/// neither the desktop nor real devices.
const CONFIG: &str = "[output]\nmode = \"none\"\nfallback = \"none\"\n\
                      [dbus]\nenabled = false\n\
                      [sources.fido]\nenabled = false\n";
/// [`CONFIG`] with D-Bus on, so that startup waits on the session bus.
const DBUS_CONFIG: &str = "[output]\nmode = \"none\"\nfallback = \"none\"\n\
                           [sources.fido]\nenabled = false\n";

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Errno(#[from] rustix::io::Errno),
    #[error("{0}")]
    Failed(String),
}

/// Waits until `done` returns `Some`, the child exits, or `timeout` passes.
fn wait_for<T>(
    child: &mut Child,
    timeout: Duration,
    mut done: impl FnMut(&mut Child) -> Result<Option<T>, TestError>,
) -> Result<Option<T>, TestError> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(value) = done(child)? {
            return Ok(Some(value));
        }
        sleep(POLL);
    }
    Ok(None)
}

fn stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut pipe) = child.stderr.take()
        && let Err(error) = pipe.read_to_string(&mut text)
    {
        text.push_str("<cannot read stderr: ");
        text.push_str(&error.to_string());
        text.push('>');
    }
    text
}

/// Temporary `HOME`, config and runtime directories for one daemon.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new(config: &str) -> Result<Self, TestError> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("config/touchcue"))?;
        fs::create_dir(root.path().join("run"))?;
        fs::write(root.path().join("config/touchcue/config.toml"), config)?;
        Ok(Self { root })
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join(name)
    }

    /// Starts `touchcue run` with the session bus at `bus`.
    fn spawn(&self, bus: &Path) -> Result<Child, TestError> {
        Ok(Command::new(env!("CARGO_BIN_EXE_touchcue"))
            .arg("run")
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus.display()),
            )
            .env("TOUCHCUE_LOG", "info")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?)
    }
}

/// Runs `test` on `child`, killing the child and attaching its stderr on failure.
fn supervise(
    mut child: Child,
    test: impl FnOnce(&mut Child) -> Result<(), TestError>,
) -> Result<(), TestError> {
    let result = test(&mut child);
    if let Err(_failed) = &result
        && child.try_wait()?.is_none()
    {
        child.kill()?;
        child.wait()?;
    }
    result.map_err(|error| TestError::Failed(format!("{error}\nstderr:\n{}", stderr(&mut child))))
}

#[test]
fn sigterm_stops_the_daemon_and_removes_its_socket() -> Result<(), TestError> {
    let env = Env::new(CONFIG)?;
    let socket = env.path("run/touchcue/events.sock");
    let child = env.spawn(Path::new("/nonexistent"))?;
    supervise(child, |child| run_test(child, &socket))
}

#[test]
fn sigterm_stops_the_daemon_while_the_session_bus_hangs() -> Result<(), TestError> {
    let env = Env::new(DBUS_CONFIG)?;
    let bus_path = env.path("bus");
    // Never answers, so the D-Bus handshake waits until the daemon gives up.
    let bus = UnixListener::bind(&bus_path)?;
    bus.set_nonblocking(true)?;
    let child = env.spawn(&bus_path)?;
    supervise(child, |child| {
        let connected = wait_for(child, START_TIMEOUT, |child| {
            if let Some(status) = child.try_wait()? {
                return Err(TestError::Failed(format!("daemon exited early: {status}")));
            }
            match bus.accept() {
                Ok((stream, _)) => Ok(Some(stream)),
                Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error.into()),
            }
        })?;
        // Held open so that the daemon sees a silent bus, not a closed one.
        let Some(_stream) = connected else {
            return Err(TestError::Failed(
                "daemon did not connect to the session bus".to_owned(),
            ));
        };
        terminate(child, STARTUP_STOP_TIMEOUT)
    })
}

fn run_test(child: &mut Child, socket: &Path) -> Result<(), TestError> {
    let started = wait_for(child, START_TIMEOUT, |child| {
        if let Some(status) = child.try_wait()? {
            return Err(TestError::Failed(format!("daemon exited early: {status}")));
        }
        Ok(socket.exists().then_some(()))
    })?;
    if started.is_none() {
        return Err(TestError::Failed("socket was not created".to_owned()));
    }

    terminate(child, STOP_TIMEOUT)?;
    if socket.exists() {
        return Err(TestError::Failed("socket left behind".to_owned()));
    }
    Ok(())
}

/// Sends SIGTERM and expects a successful exit within `timeout`.
fn terminate(child: &mut Child, timeout: Duration) -> Result<(), TestError> {
    let pid = i32::try_from(child.id())
        .map_err(|error| TestError::Failed(format!("child pid out of range: {error}")))
        .map(Pid::from_raw)?
        .ok_or_else(|| TestError::Failed("invalid child pid".to_owned()))?;
    let signalled = Instant::now();
    kill_process(pid, Signal::TERM)?;
    let status: Option<ExitStatus> = wait_for(child, timeout, |child| Ok(child.try_wait()?))?;
    let Some(status) = status else {
        return Err(TestError::Failed(format!(
            "daemon still running {timeout:?} after SIGTERM"
        )));
    };
    let elapsed = signalled.elapsed();
    if !status.success() {
        return Err(TestError::Failed(format!("daemon exited with {status}")));
    }
    eprintln!("daemon exited {elapsed:?} after SIGTERM");
    Ok(())
}
