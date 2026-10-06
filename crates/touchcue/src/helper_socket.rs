//! Client side of the daemon's helper socket, used by the reporters
//! `touchcue scdaemon` and `touchcue askpass`.

use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use touchcue_core::helper::Message;

/// Daemon helper socket, relative to `$XDG_RUNTIME_DIR`.
pub const SOCKET_PATH: &str = "touchcue/helper.sock";

/// Longest wait to connect to the daemon or to write one line to it.
pub const SOCKET_TIMEOUT: Duration = Duration::from_millis(200);

/// Failure to reach the daemon or to write to it.
#[derive(Debug, thiserror::Error)]
pub enum HelperSocketError {
    #[error("daemon helper socket I/O failed")]
    Io(#[source] io::Error),
    #[error("daemon helper socket timed out")]
    TimedOut(#[source] Elapsed),
}

/// Connects to the helper socket at `path` within [`SOCKET_TIMEOUT`].
///
/// # Errors
///
/// Returns [`HelperSocketError`] when the connection fails or times out.
#[tracing::instrument(skip_all, fields(path = %path.display()))]
pub async fn connect(path: &Path) -> Result<UnixStream, HelperSocketError> {
    timeout(SOCKET_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(HelperSocketError::TimedOut)?
        .map_err(HelperSocketError::Io)
}

/// Writes `message` as one line within [`SOCKET_TIMEOUT`].
///
/// # Errors
///
/// Returns [`HelperSocketError`] when the write fails or times out; the
/// line may then be partly written.
#[tracing::instrument(skip_all)]
pub async fn send(stream: &mut UnixStream, message: &Message) -> Result<(), HelperSocketError> {
    timeout(
        SOCKET_TIMEOUT,
        stream.write_all(message.encode().as_bytes()),
    )
    .await
    .map_err(HelperSocketError::TimedOut)?
    .map_err(HelperSocketError::Io)
}
