//! Error type shared by this crate's tests.

use thiserror::Error;

use crate::config::ConfigError;
use crate::template::{RenderError, TemplateError};

/// Failure of a test step.
#[derive(Debug, Error)]
pub(crate) enum TestError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Unexpected(String),
    #[error("{0}")]
    Missing(&'static str),
}

impl From<String> for TestError {
    fn from(message: String) -> Self {
        Self::Unexpected(message)
    }
}

impl From<&'static str> for TestError {
    fn from(message: &'static str) -> Self {
        Self::Missing(message)
    }
}

pub(crate) type TestResult<T = ()> = Result<T, TestError>;
