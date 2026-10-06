//! Resolution of desktop entry icon names to files.

use std::fs;
use std::path::Path;

use tracing::debug;

use super::bounded::logged;

/// Image formats accepted for an absolute icon path, compared ignoring ASCII case.
const IMAGE_EXTENSIONS: &[&str] = &["png", "svg", "xpm"];

/// Returns the file for `icon`, an absolute path or a theme icon name, at `size` pixels.
///
/// An absolute path is returned only when it is a regular file, after
/// following symlinks, with a `.png`, `.svg` or `.xpm` extension. A name is
/// looked up in the hicolor theme and the standard fallbacks. A value
/// containing control characters, a relative path containing `/`, and a
/// non-UTF-8 result yield `None`.
#[must_use]
pub fn resolve_icon(icon: &str, size: u16) -> Option<String> {
    if icon.is_empty() || icon.chars().any(char::is_control) {
        return None;
    }
    let path = Path::new(icon);
    if path.is_absolute() {
        let image = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                IMAGE_EXTENSIONS
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known))
            });
        let regular = match fs::metadata(path) {
            Ok(meta) => meta.is_file(),
            Err(error) => {
                debug!(path = %logged(path), error = &error as &dyn std::error::Error, "icon file unreadable");
                false
            }
        };
        return (image && regular).then(|| icon.to_owned());
    }
    if icon.contains('/') {
        return None;
    }
    let found = freedesktop_icons::lookup(icon)
        .with_size(size)
        .with_cache()
        .find()?;
    match found.into_os_string().into_string() {
        Ok(path) => Some(path),
        Err(path) => {
            debug!(path = %logged(Path::new(&path)), "icon path is not UTF-8");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::linux::test_error::{TestError, TestResult};

    #[test]
    fn absolute_path_must_be_regular_image() -> TestResult {
        let dir = tempfile::tempdir()?;
        let root = dir
            .path()
            .to_str()
            .ok_or(TestError::Missing("non-UTF-8 temp path"))?;
        let png = format!("{root}/app.PNG");
        fs::write(&png, b"png")?;
        let text = format!("{root}/app.txt");
        fs::write(&text, b"txt")?;
        let folder = format!("{root}/folder.svg");
        fs::create_dir(&folder)?;
        let fifo = format!("{root}/fifo.png");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            fifo.as_str(),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        assert_eq!(resolve_icon(&png, 64).as_deref(), Some(png.as_str()));
        assert_eq!(resolve_icon(&text, 64), None);
        assert_eq!(resolve_icon(&folder, 64), None);
        assert_eq!(resolve_icon(&fifo, 64), None);
        assert_eq!(resolve_icon("/nonexistent/touchcue-icon.png", 64), None);
        assert_eq!(resolve_icon(&format!("{png}\n"), 64), None);
        Ok(())
    }

    #[test]
    fn rejects_relative_paths_and_empty_names() {
        assert_eq!(resolve_icon("icons/app.png", 64), None);
        assert_eq!(resolve_icon("", 64), None);
    }
}
