//! Hook commands on targets where running them is not supported.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::runner::{Ending, Finished, Stop};

/// Write end of a command's stdin pipe; never passed to a command here.
pub(crate) type Stdin = tokio::io::Sink;

/// Failure to run a hook command.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("hook commands are not supported on this platform")]
    Unsupported,
}

#[expect(clippy::unused_async, reason = "matches the Linux runner")]
pub(crate) async fn run(
    _command: &[String],
    _env: &[(String, String)],
    _timeout: Duration,
    _kill: &CancellationToken,
) -> Result<Finished, RunError> {
    Err(RunError::Unsupported)
}

#[expect(clippy::unused_async, reason = "matches the Linux runner")]
pub(crate) async fn supervised<F, U>(
    _command: &[String],
    _env: &[(String, String)],
    _piped: bool,
    _stop: Stop,
    _until: F,
) -> Result<Finished, RunError>
where
    F: FnOnce(Option<Stdin>) -> U,
    U: Future<Output = Ending>,
{
    Err(RunError::Unsupported)
}
