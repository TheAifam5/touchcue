//! Output backends of targets without a supported desktop integration.

use std::future::Ready;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{Backend, Capabilities, Command, UiConfig, UiError};

/// Never constructed: [`spawn`] always returns [`Backend::Off`] without a task.
#[derive(Debug)]
pub(crate) enum Handle {}

impl Handle {
    pub(crate) fn send(&self, _cmd: Command) -> Ready<Result<(), UiError>> {
        match *self {}
    }

    pub(crate) fn shutdown(self, _deadline: Instant) -> Ready<Result<(), UiError>> {
        match self {}
    }
}

#[expect(clippy::unused_async, reason = "matches the Linux backend")]
pub(crate) async fn spawn(
    _cfg: UiConfig,
    _cancel: CancellationToken,
) -> Result<(Backend, Option<Handle>), UiError> {
    Ok((Backend::Off, None))
}

#[expect(clippy::unused_async, reason = "matches the Linux backend")]
pub(crate) async fn probe() -> Capabilities {
    Capabilities::default()
}
