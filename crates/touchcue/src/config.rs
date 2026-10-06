//! Location and loading of the configuration file.

use std::ffi::OsString;
use std::fmt::{self, Display};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use miette::{
    Diagnostic, GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, Severity,
    SourceCode,
};
use touchcue_core::{Config, ConfigError};

/// Largest configuration file read, in bytes.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Location of the configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPath {
    pub path: PathBuf,
    /// Given with `--config`; such a file must exist.
    pub explicit: bool,
}

/// Returns the configuration path: `explicit`, else
/// `$XDG_CONFIG_HOME/touchcue/config.toml`, else
/// `$HOME/.config/touchcue/config.toml`.
///
/// Empty environment values count as unset. Returns `None` when no path is
/// given and neither variable is set.
pub fn path(
    explicit: Option<PathBuf>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<ConfigPath> {
    if let Some(path) = explicit {
        return Some(ConfigPath {
            path,
            explicit: true,
        });
    }
    let nonempty = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = nonempty(xdg_config_home).or_else(|| nonempty(home).map(|h| h.join(".config")))?;
    Some(ConfigPath {
        path: base.join("touchcue").join("config.toml"),
        explicit: false,
    })
}

/// Configuration read from a file or defaulted.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// Whether the file existed; `false` means the defaults are in use.
    pub found: bool,
}

/// Failure to read or validate the configuration file.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("configuration file {} does not exist", path.display())]
    NotFound { path: PathBuf },
    #[error("cannot read configuration file {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configuration file {} is larger than {MAX_CONFIG_BYTES} bytes", path.display())]
    TooLarge { path: PathBuf },
    #[error("configuration file {} is not valid UTF-8", path.display())]
    NotUtf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    /// `position` is the 1-based line and column of the error span start;
    /// `text` holds the file's name and contents for diagnostics.
    #[error("invalid configuration file {}{}", path.display(), at(*position))]
    Invalid {
        path: PathBuf,
        position: Option<(usize, usize)>,
        text: Box<NamedSource<String>>,
        #[source]
        source: Box<ConfigError>,
    },
}

impl LoadError {
    fn config_error(&self) -> Option<&ConfigError> {
        match self {
            Self::Invalid { source, .. } => Some(source),
            Self::NotFound { .. }
            | Self::Io { .. }
            | Self::TooLarge { .. }
            | Self::NotUtf8 { .. } => None,
        }
    }
}

/// Forwards the code, help and labels of a [`ConfigError`] and supplies the
/// file contents, so the labels point into the file.
impl Diagnostic for LoadError {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.config_error()?.code()
    }

    fn severity(&self) -> Option<Severity> {
        self.config_error()?.severity()
    }

    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.config_error()?.help()
    }

    fn url<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.config_error()?.url()
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        match self {
            Self::Invalid { text, .. } => Some(&**text),
            Self::NotFound { .. }
            | Self::Io { .. }
            | Self::TooLarge { .. }
            | Self::NotUtf8 { .. } => None,
        }
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        self.config_error()?.labels()
    }
}

/// Returns the report handler for configuration errors: graphical, with
/// colors only when `color` is set.
#[must_use]
pub fn report_handler(color: bool) -> GraphicalReportHandler {
    let theme = if color {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::unicode_nocolor()
    };
    GraphicalReportHandler::new_themed(theme)
}

/// Renders `error` as a report with its source snippet and labels.
///
/// # Errors
///
/// Returns [`fmt::Error`] when the handler fails to format the report.
pub fn render(error: &LoadError, handler: &GraphicalReportHandler) -> Result<String, fmt::Error> {
    let mut report = String::new();
    handler.render_report(&mut report, error)?;
    Ok(report)
}

fn at(position: Option<(usize, usize)>) -> String {
    position.map_or_else(String::new, |(line, col)| format!(":{line}:{col}"))
}

/// Reads and validates the configuration at `path`.
///
/// `None` and a missing implicit file yield the defaults.
///
/// # Errors
///
/// Returns [`LoadError`] when an explicit file is missing, or the file cannot
/// be read, exceeds 1 MiB, is not UTF-8 or is not a valid configuration.
#[tracing::instrument(
    skip_all,
    fields(path = path.map(|p| tracing::field::display(p.path.display())))
)]
pub fn load(path: Option<&ConfigPath>) -> Result<Loaded, LoadError> {
    let Some(ConfigPath { path, explicit }) = path else {
        return Ok(defaults());
    };
    let text = match read_limited(path) {
        Ok(text) => text,
        Err(LoadError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            if *explicit {
                return Err(LoadError::NotFound { path: path.clone() });
            }
            tracing::debug!("no configuration file, using defaults");
            return Ok(defaults());
        }
        Err(error) => return Err(error),
    };
    let config = match Config::from_toml(&text) {
        Ok(config) => config,
        Err(source) => {
            return Err(LoadError::Invalid {
                path: path.clone(),
                position: source.span().map(|span| line_col(&text, span.start)),
                text: Box::new(NamedSource::new(path.display().to_string(), text)),
                source: Box::new(source),
            });
        }
    };
    tracing::debug!("configuration loaded");
    Ok(Loaded {
        config,
        found: true,
    })
}

fn defaults() -> Loaded {
    Loaded {
        config: Config::default(),
        found: false,
    }
}

fn read_limited(path: &Path) -> Result<String, LoadError> {
    let io_error = |source| LoadError::Io {
        path: path.to_owned(),
        source,
    };
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    match u64::try_from(bytes.len()) {
        Ok(len) if len <= MAX_CONFIG_BYTES => {}
        // A length that does not fit in u64 is past the limit as well.
        Ok(_) | Err(_) => {
            return Err(LoadError::TooLarge {
                path: path.to_owned(),
            });
        }
    }
    String::from_utf8(bytes).map_err(|error| LoadError::NotUtf8 {
        path: path.to_owned(),
        source: error.utf8_error(),
    })
}

/// Returns the 1-based line and column, in chars, of byte `offset` in `text`.
///
/// An offset past the end or inside a character is moved back to the
/// nearest character boundary.
pub fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let before = text.get(..end).unwrap_or_default();
    let line_start = before.rfind('\n').map_or(0, |i| i.saturating_add(1));
    let line = before.matches('\n').count().saturating_add(1);
    let col = before
        .get(line_start..)
        .unwrap_or_default()
        .chars()
        .count()
        .saturating_add(1);
    (line, col)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] io::Error),
        #[error(transparent)]
        Load(#[from] LoadError),
        #[error(transparent)]
        Size(#[from] std::num::TryFromIntError),
        #[error("expected LoadError::Invalid")]
        NotInvalid,
        #[error("the configuration error has no diagnostic code")]
        NoCode,
        #[error(transparent)]
        Render(#[from] fmt::Error),
    }

    type TestResult = Result<(), TestError>;

    fn implicit(path: PathBuf) -> ConfigPath {
        ConfigPath {
            path,
            explicit: false,
        }
    }

    fn explicit(path: PathBuf) -> ConfigPath {
        ConfigPath {
            path,
            explicit: true,
        }
    }

    #[test]
    fn explicit_path_wins() {
        assert_eq!(
            path(
                Some("/etc/t.toml".into()),
                Some("/x".into()),
                Some("/h".into())
            ),
            Some(explicit("/etc/t.toml".into()))
        );
    }

    #[test]
    fn xdg_config_home_precedes_home() {
        assert_eq!(
            path(None, Some("/x".into()), Some("/h".into())),
            Some(implicit("/x/touchcue/config.toml".into()))
        );
    }

    #[test]
    fn empty_xdg_config_home_falls_back_to_home() {
        assert_eq!(
            path(None, Some("".into()), Some("/h".into())),
            Some(implicit("/h/.config/touchcue/config.toml".into()))
        );
        assert_eq!(path(None, None, None), None);
    }

    #[test]
    fn line_col_counts_lines_and_chars() {
        let text = "a = 1\nzé = [\n";
        assert_eq!(line_col(text, 0), (1, 1));
        assert_eq!(line_col(text, 6), (2, 1));
        // byte 9 is after "zé", which is 3 bytes but 2 chars
        assert_eq!(line_col(text, 9), (2, 3));
        assert_eq!(line_col(text, 8), (2, 2));
        assert_eq!(line_col(text, text.len()), (3, 1));
        assert_eq!(line_col(text, 999), (3, 1));
    }

    #[test]
    fn missing_implicit_file_yields_defaults() -> TestResult {
        let dir = tempfile::tempdir()?;
        let loaded = load(Some(&implicit(dir.path().join("absent.toml"))))?;
        assert!(!loaded.found);
        assert_eq!(loaded.config, Config::default());
        Ok(())
    }

    #[test]
    fn missing_explicit_file_is_an_error() -> TestResult {
        let dir = tempfile::tempdir()?;
        let result = load(Some(&explicit(dir.path().join("absent.toml"))));
        assert!(matches!(result, Err(LoadError::NotFound { .. })));
        Ok(())
    }

    #[test]
    fn invalid_file_reports_position() -> TestResult {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        fs::write(&file, "[popup]\nposition = \"middle\"\n")?;
        let error = match load(Some(&explicit(file.clone()))) {
            Err(error @ LoadError::Invalid { .. }) => error,
            Err(other) => return Err(other.into()),
            Ok(_loaded) => return Err(TestError::NotInvalid),
        };
        assert_eq!(
            error.to_string(),
            format!("invalid configuration file {}:2:12", file.display())
        );
        Ok(())
    }

    #[test]
    fn invalid_file_renders_a_report_with_the_label() -> TestResult {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        fs::write(&file, "[popup]\nposition = \"middle\"\n")?;
        let error = match load(Some(&explicit(file.clone()))) {
            Err(error @ LoadError::Invalid { .. }) => error,
            Err(other) => return Err(other.into()),
            Ok(_loaded) => return Err(TestError::NotInvalid),
        };
        let handler = GraphicalReportHandler::new_themed(GraphicalTheme::none()).with_width(80);
        let report = render(&error, &handler)?;
        assert!(report.contains(&file.display().to_string()), "{report}");
        assert!(report.contains("position = \"middle\""), "{report}");
        let code = error
            .code()
            .map(|code| code.to_string())
            .ok_or(TestError::NoCode)?;
        assert!(report.contains(&code), "{report}");
        Ok(())
    }

    #[test]
    fn oversized_file_is_rejected() -> TestResult {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("config.toml");
        let size = usize::try_from(MAX_CONFIG_BYTES)?.saturating_add(1);
        fs::write(&file, "#".repeat(size))?;
        assert!(matches!(
            load(Some(&implicit(file))),
            Err(LoadError::TooLarge { .. })
        ));
        Ok(())
    }
}
