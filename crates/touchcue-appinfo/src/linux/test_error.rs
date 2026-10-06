//! Error type shared by this module's tests.

use std::num::TryFromIntError;

use thiserror::Error;

/// Failure of a test step.
#[derive(Debug, Error)]
pub(super) enum TestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Errno(#[from] rustix::io::Errno),
    #[error(transparent)]
    TryFromInt(#[from] TryFromIntError),
    #[error("{0}")]
    Missing(&'static str),
    #[error("path {0:?} is not UTF-8")]
    NonUtf8Path(std::ffi::OsString),
}

pub(super) type TestResult<T = ()> = Result<T, TestError>;
