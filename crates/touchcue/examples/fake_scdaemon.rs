//! Stand-in for scdaemon in `--multi-server` mode, used by the wrapper's
//! integration tests: it listens on the socket named by
//! `FAKE_SCDAEMON_SOCKET` before answering on stdin and stdout, as scdaemon
//! does. On the socket, `PKSIGN` is answered after 600 ms with a data line
//! and `OK`; every other line gets `OK`. At EOF on stdin it removes the
//! socket path and exits with 3.

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() -> Result<std::process::ExitCode, unix::FakeError> {
    unix::main()
}

#[cfg(unix)]
mod unix {
    use std::io::{self, BufRead as _, BufReader, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::thread;
    use std::time::Duration;

    /// Environment variable naming the socket path to listen on.
    const SOCKET_ENV: &str = "FAKE_SCDAEMON_SOCKET";
    /// Silence before the answer to `PKSIGN`, longer than the show delay.
    const SIGN_DELAY: Duration = Duration::from_millis(600);

    #[derive(Debug, thiserror::Error)]
    pub(super) enum FakeError {
        #[error("{SOCKET_ENV} is not set")]
        NoSocket,
        #[error(transparent)]
        Io(#[from] io::Error),
    }

    pub(super) fn main() -> Result<ExitCode, FakeError> {
        let socket = std::env::var_os(SOCKET_ENV)
            .map(PathBuf::from)
            .ok_or(FakeError::NoSocket)?;
        let listener = UnixListener::bind(&socket)?;
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        thread::spawn(move || {
                            if let Err(error) = serve(stream) {
                                eprintln!("fake scdaemon connection: {error}");
                            }
                        });
                    }
                    Err(error) => eprintln!("fake scdaemon accept: {error}"),
                }
            }
        });

        let mut stdout = io::stdout().lock();
        stdout.write_all(b"OK Pleased to meet you\n")?;
        stdout.flush()?;
        for line in io::stdin().lock().lines() {
            line?;
            stdout.write_all(b"OK\n")?;
            stdout.flush()?;
        }
        std::fs::remove_file(&socket)?;
        Ok(ExitCode::from(3))
    }

    fn serve(stream: UnixStream) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        for line in BufReader::new(stream).lines() {
            if line?.starts_with("PKSIGN") {
                thread::sleep(SIGN_DELAY);
                writer.write_all(b"D \x01\xff\nOK\n")?;
            } else {
                writer.write_all(b"OK\n")?;
            }
        }
        Ok(())
    }
}
