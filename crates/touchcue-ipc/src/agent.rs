//! Read-only queries of gpg-agent: its socket paths from `gpgconf` and the
//! `OpenPGP` card attributes through its Assuan socket.
//!
//! Only `KEYINFO --list` and, when it lists a card key, `SCD GETATTR UIF-n`
//! are sent. The raw replies, including keygrips and card serial numbers,
//! stay in memory until [`read_card`] returns and are never logged.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::timeout;
use touchcue_core::assuan::{self, CardKey, LineError, Response, SLOTS, Uif};

/// Longest wait for one Assuan command and its reply, and for `gpgconf`.
pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(2);
/// Most lines accepted in reply to one command.
const MAX_REPLY_LINES: usize = 1024;
/// Largest `gpgconf --list-dirs` output read, in bytes.
const MAX_GPGCONF_OUTPUT: u64 = 64 * 1024;
/// Largest response line read, in bytes: [`assuan::MAX_LINE`] plus CR and LF.
const MAX_READ: u64 = 64 * 1024 + 2;

/// Failure to query gpg-agent or gpgconf.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{context}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{context} timed out")]
    TimedOut {
        context: &'static str,
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("gpgconf failed")]
    Gpgconf,
    #[error("gpgconf did not list {name}")]
    MissingDir { name: &'static str },
    #[error("gpgconf listed an invalid {name}")]
    BadDir {
        name: &'static str,
        #[source]
        source: LineError,
    },
    #[error("gpgconf listed a relative {name}")]
    RelativeDir { name: &'static str },
    #[error("gpg-agent socket belongs to another user")]
    ForeignAgent,
    #[error("invalid reply from gpg-agent")]
    Reply(#[from] LineError),
    #[error("gpg-agent reply exceeded {MAX_REPLY_LINES} lines")]
    TooLong,
    #[error("gpg-agent closed the connection")]
    Closed,
    #[error("gpg-agent asked for input")]
    Inquire,
    #[error("gpg-agent rejected the greeting with error {code}")]
    Greeting { code: u32 },
}

/// Locations reported by `gpgconf --list-dirs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    /// `S.gpg-agent`.
    pub agent: PathBuf,
    /// `S.gpg-agent.ssh`.
    pub ssh: PathBuf,
    /// The `GnuPG` home directory, holding `gpg-agent.conf`.
    pub homedir: PathBuf,
}

/// Runs `gpgconf --list-dirs` and returns the agent sockets and home directory.
///
/// `gpgconf` does not start gpg-agent. It is killed if it takes longer
/// than [`EXCHANGE_TIMEOUT`].
///
/// # Errors
///
/// Returns [`AgentError`] when `gpgconf` cannot run, fails, times out, or
/// does not list the three entries.
#[tracing::instrument(level = "debug", skip_all, err)]
pub async fn paths() -> Result<AgentPaths, AgentError> {
    let mut child = Command::new("gpgconf")
        .arg("--list-dirs")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(io("cannot run gpgconf"))?;
    let stdout = child.stdout.take().ok_or(AgentError::Gpgconf)?;
    let run = async {
        let mut out = Vec::new();
        stdout
            .take(MAX_GPGCONF_OUTPUT)
            .read_to_end(&mut out)
            .await
            .map_err(io("cannot read gpgconf output"))?;
        let status = child.wait().await.map_err(io("cannot wait for gpgconf"))?;
        if !status.success() {
            return Err(AgentError::Gpgconf);
        }
        Ok(out)
    };
    let out = timeout(EXCHANGE_TIMEOUT, run)
        .await
        .map_err(|source| AgentError::TimedOut {
            context: "gpgconf",
            source,
        })??;
    parse_dirs(&String::from_utf8_lossy(&out))
}

/// Parses `name:value` lines of `gpgconf --list-dirs`, whose values are percent-escaped.
fn parse_dirs(out: &str) -> Result<AgentPaths, AgentError> {
    let get = |name: &'static str| -> Result<PathBuf, AgentError> {
        let value = out
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .ok_or(AgentError::MissingDir { name })?;
        // gpgconf escapes with `%XX` only; a `+` is literal.
        let value = assuan::decode_status(value.replace('+', "%2B").as_bytes())
            .map_err(|source| AgentError::BadDir { name, source })?;
        let path = PathBuf::from(OsString::from_vec(value));
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(AgentError::RelativeDir { name })
        }
    };
    Ok(AgentPaths {
        agent: get("agent-socket")?,
        ssh: get("agent-ssh-socket")?,
        homedir: get("homedir")?,
    })
}

/// `OpenPGP` card attributes, per key slot 1 to 3.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardState {
    /// UIF per slot; `None` when the card did not report it.
    pub uif: [Option<Uif>; SLOTS],
    /// Manufacturer ID, when every card key listed agrees on one.
    pub manufacturer: Option<u16>,
}

/// Reads the card keys and the UIF of every slot from gpg-agent at `socket`.
///
/// Each command and its reply get [`EXCHANGE_TIMEOUT`]. A command that
/// fails with `ERR` leaves its value unknown; the card may be absent.
///
/// # Errors
///
/// Returns [`AgentError`] when the socket cannot be reached, belongs to
/// another user, an exchange times out, or the agent sends an invalid,
/// oversized or interactive reply.
#[tracing::instrument(level = "debug", skip_all, err)]
pub async fn read_card(socket: &Path) -> Result<CardState, AgentError> {
    let stream = timeout(EXCHANGE_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|source| AgentError::TimedOut {
            context: "connecting to gpg-agent",
            source,
        })?
        .map_err(io("cannot connect to gpg-agent"))?;
    let cred = stream
        .peer_cred()
        .map_err(io("cannot read gpg-agent credentials"))?;
    if cred.uid() != rustix::process::geteuid().as_raw() {
        return Err(AgentError::ForeignAgent);
    }
    let mut session = Session {
        stream: BufReader::new(stream),
    };
    if let Err(code) = session.exchange(None).await?.result {
        return Err(AgentError::Greeting { code });
    }
    let mut state = CardState::default();
    let keys = session.exchange(Some("KEYINFO --list")).await?;
    let card_keys: Vec<CardKey> = keys
        .status
        .iter()
        .filter(|(keyword, _)| keyword == "KEYINFO")
        .filter_map(|(_, args)| match assuan::parse_keyinfo(args) {
            Ok(key) => key,
            Err(error) => {
                tracing::trace!(
                    error = &error as &dyn std::error::Error,
                    "KEYINFO line skipped"
                );
                None
            }
        })
        .collect();
    if card_keys.is_empty() {
        // `SCD` commands would start scdaemon and claim the card reader.
        tracing::debug!("no card key listed; UIF not read");
        bye(&mut session).await;
        return Ok(state);
    }
    let manufacturers: Vec<Option<u16>> = card_keys.iter().map(|key| key.manufacturer).collect();
    state.manufacturer = manufacturers
        .first()
        .copied()
        .flatten()
        .filter(|first| manufacturers.iter().all(|m| *m == Some(*first)));
    for (index, uif) in state.uif.iter_mut().enumerate() {
        let slot = index.saturating_add(1);
        let keyword = format!("UIF-{slot}");
        let reply = session
            .exchange(Some(&format!("SCD GETATTR {keyword}")))
            .await?;
        if let Err(code) = reply.result {
            tracing::debug!(slot, code, "card did not report UIF");
        }
        *uif = match reply.status.iter().find(|(k, _)| *k == keyword) {
            Some((_, args)) => match Uif::parse(args) {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::debug!(
                        slot,
                        error = &error as &dyn std::error::Error,
                        "invalid UIF value"
                    );
                    None
                }
            },
            None => None,
        };
    }
    bye(&mut session).await;
    Ok(state)
}

/// Ends the session; the agent also closes the connection when the stream
/// drops, so a failed `BYE` changes nothing.
async fn bye(session: &mut Session) {
    if let Err(error) = session.exchange(Some("BYE")).await {
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            "gpg-agent did not acknowledge BYE"
        );
    }
}

/// Status lines and the final result of one command.
struct Reply {
    status: Vec<(String, Vec<u8>)>,
    /// `Err` holds the gpg-error code.
    result: Result<(), u32>,
}

struct Session {
    stream: BufReader<UnixStream>,
}

impl Session {
    /// Sends `command`, if any, and reads lines up to `OK` or `ERR` within [`EXCHANGE_TIMEOUT`].
    async fn exchange(&mut self, command: Option<&str>) -> Result<Reply, AgentError> {
        timeout(EXCHANGE_TIMEOUT, self.exchange_unbounded(command))
            .await
            .map_err(|source| AgentError::TimedOut {
                context: "gpg-agent exchange",
                source,
            })?
    }

    async fn exchange_unbounded(&mut self, command: Option<&str>) -> Result<Reply, AgentError> {
        if let Some(command) = command {
            let line = format!("{command}\n");
            self.stream
                .get_mut()
                .write_all(line.as_bytes())
                .await
                .map_err(io("cannot write to gpg-agent"))?;
        }
        let mut status = Vec::new();
        let mut line = Vec::new();
        for _ in 0..MAX_REPLY_LINES {
            line.clear();
            let read = (&mut self.stream)
                .take(MAX_READ)
                .read_until(b'\n', &mut line)
                .await
                .map_err(io("cannot read from gpg-agent"))?;
            if read == 0 {
                return Err(AgentError::Closed);
            }
            if line.last() != Some(&b'\n') {
                return Err(AgentError::Reply(LineError::TooLong));
            }
            match assuan::parse_line(&line)? {
                Response::Ok => {
                    return Ok(Reply {
                        status,
                        result: Ok(()),
                    });
                }
                Response::Err { code } => {
                    return Ok(Reply {
                        status,
                        result: Err(code),
                    });
                }
                Response::Status { keyword, args } => status.push((keyword, args)),
                Response::Data(_) | Response::Comment => {}
                Response::Inquire => return Err(AgentError::Inquire),
            }
        }
        Err(AgentError::TooLong)
    }
}

fn io(context: &'static str) -> impl FnOnce(std::io::Error) -> AgentError {
    move |source| AgentError::Io { context, source }
}

#[cfg(test)]
pub(crate) mod tests {
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    pub(crate) enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Agent(#[from] AgentError),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
    }

    const GRIP: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
    const SERIAL: &str = "D2760001240100000006000000010000";

    /// Serves one connection at `path`, answering `KEYINFO --list` with
    /// one card key when `uif` is given, else with one disk key only, and
    /// `SCD GETATTR UIF-n` with `uif[n - 1]`: a value line, or an `ERR` for
    /// `None`. Returns the commands received.
    pub(crate) fn fake_agent(
        path: &Path,
        uif: Option<[Option<&'static str>; 3]>,
    ) -> Result<JoinHandle<Result<Vec<String>, TestError>>, TestError> {
        let listener = UnixListener::bind(path)?;
        Ok(tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (read, mut write) = stream.into_split();
            write.write_all(b"OK Pleased to meet you\n").await?;
            let mut lines = BufReader::new(read).lines();
            let mut received = Vec::new();
            while let Some(line) = lines.next_line().await? {
                let reply = match line.as_str() {
                    "KEYINFO --list" if uif.is_some() => {
                        format!("S KEYINFO {GRIP} T {SERIAL} OPENPGP.1 - - - - -\nOK\n")
                    }
                    "KEYINFO --list" => format!("S KEYINFO {GRIP} D - - - P - - -\nOK\n"),
                    "BYE" => "OK closing connection\n".to_owned(),
                    other => match ["1", "2", "3"]
                        .iter()
                        .position(|n| other.strip_prefix("SCD GETATTR UIF-") == Some(n))
                        .and_then(|index| uif?.get(index).copied().flatten())
                    {
                        Some(value) => format!("S {} {value}\nOK\n", &other[12..]),
                        None => "ERR 100663406 Unsupported certificate <SCD>\n".to_owned(),
                    },
                };
                received.push(line.clone());
                write.write_all(reply.as_bytes()).await?;
                if line == "BYE" {
                    break;
                }
            }
            Ok(received)
        }))
    }

    #[tokio::test]
    async fn reads_uif_and_manufacturer() -> Result<(), TestError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("S.gpg-agent");
        let agent = fake_agent(&path, Some([Some("%03+"), Some("%00+"), None]))?;

        let state = read_card(&path).await?;

        assert_eq!(state.uif, [Some(Uif::Cached), Some(Uif::Off), None]);
        assert_eq!(state.manufacturer, Some(6));
        assert_eq!(
            agent.await??,
            [
                "KEYINFO --list",
                "SCD GETATTR UIF-1",
                "SCD GETATTR UIF-2",
                "SCD GETATTR UIF-3",
                "BYE"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_card_key_sends_no_scd_command() -> Result<(), TestError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("S.gpg-agent");
        let agent = fake_agent(&path, None)?;

        let state = read_card(&path).await?;

        assert_eq!(state, CardState::default());
        assert_eq!(agent.await??, ["KEYINFO --list", "BYE"]);
        Ok(())
    }

    #[tokio::test]
    async fn silent_agent_times_out() -> Result<(), TestError> {
        tokio::time::pause();
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("S.gpg-agent");
        let _listener = UnixListener::bind(&path)?;
        let read = read_card(&path).await;
        assert!(matches!(read, Err(AgentError::TimedOut { .. })), "{read:?}");
        Ok(())
    }

    #[test]
    fn parses_gpgconf_dirs() -> Result<(), AgentError> {
        let out = "socketdir:/run/user/1000/gnupg\n\
                   agent-ssh-socket:/run/user/1000/gnupg/S.gpg-agent.ssh\n\
                   agent-socket:/run/user/1000/gnupg/S.gpg-agent\n\
                   homedir:/home/user/a%3ab+c\n";
        assert_eq!(
            parse_dirs(out)?,
            AgentPaths {
                agent: PathBuf::from("/run/user/1000/gnupg/S.gpg-agent"),
                ssh: PathBuf::from("/run/user/1000/gnupg/S.gpg-agent.ssh"),
                homedir: PathBuf::from("/home/user/a:b+c"),
            }
        );
        assert!(matches!(
            parse_dirs("agent-socket:/a\nhomedir:/h\n"),
            Err(AgentError::MissingDir {
                name: "agent-ssh-socket"
            })
        ));
        Ok(())
    }
}
