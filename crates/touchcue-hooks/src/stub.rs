//! Hook commands on targets where running them is not supported.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::runner::Finished;

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
