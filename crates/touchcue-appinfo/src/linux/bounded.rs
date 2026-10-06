//! Size-bounded reads of regular files.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};
use touchcue_core::text::sanitize;
use tracing::trace;

/// Longest path text recorded in logs, in chars.
const LOGGED_PATH_MAX: usize = 256;

/// Returns the UTF-8 content of the regular file at `path`.
///
/// Returns `None` when the file cannot be opened, is not a regular file after
/// following symlinks, is larger than `max` bytes, or is not UTF-8. Opening
/// never blocks on a FIFO and never acquires a controlling terminal.
pub(crate) fn read_bounded(path: &Path, max: usize) -> Option<String> {
    let bytes = read_prefix(path, max.checked_add(1)?)?;
    if bytes.len() > max {
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) => {
            trace!(path = %logged(path), error = &error as &dyn std::error::Error, "file is not UTF-8");
            None
        }
    }
}

/// Returns the first `max` bytes of the regular file at `path`, decoded lossily.
///
/// The result is at most `max` bytes, cut on a char boundary. Fails like
/// [`read_bounded`] except that size and encoding never cause `None`.
pub(crate) fn read_truncated(path: &Path, max: usize) -> Option<String> {
    let bytes = read_prefix(path, max)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    text.truncate(text.floor_char_boundary(max));
    Some(text)
}

fn read_prefix(path: &Path, limit: usize) -> Option<Vec<u8>> {
    match try_read_prefix(path, limit) {
        Ok(bytes) => bytes,
        Err(error) => {
            trace!(path = %logged(path), error = &error as &dyn std::error::Error, "bounded read failed");
            None
        }
    }
}

/// Returns `Ok(None)` for a file that is not regular after following symlinks.
fn try_read_prefix(path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::empty())?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let limit =
        u64::try_from(limit).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    File::from(fd).take(limit).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Returns `path` as sanitized text for log fields.
pub(crate) fn logged(path: &Path) -> String {
    sanitize(&path.to_string_lossy(), LOGGED_PATH_MAX).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    use super::*;

    use crate::linux::test_error::TestResult;

    #[test]
    fn enforces_cap() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.desktop");
        fs::write(&path, "x".repeat(16))?;
        assert_eq!(read_bounded(&path, 16).map(|t| t.len()), Some(16));
        assert_eq!(read_bounded(&path, 15), None);
        assert_eq!(read_truncated(&path, 4).as_deref(), Some("xxxx"));
        fs::write(&path, b"ab\xff")?;
        assert_eq!(read_bounded(&path, 16), None);
        assert_eq!(read_truncated(&path, 16).as_deref(), Some("ab\u{fffd}"));
        fs::write(&path, "aé")?;
        assert_eq!(read_truncated(&path, 2).as_deref(), Some("a"));
        assert_eq!(read_bounded(&dir.path().join("missing"), 16), None);
        assert_eq!(read_bounded(dir.path(), 16), None);
        Ok(())
    }

    #[test]
    fn rejects_special_files_without_blocking() -> TestResult {
        let dir = tempfile::tempdir()?;
        let fifo = dir.path().join("a.desktop");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR)?;
        let zero = dir.path().join("b.desktop");
        symlink("/dev/zero", &zero)?;
        let start = Instant::now();
        assert_eq!(read_bounded(&fifo, 64 * 1024), None);
        assert_eq!(read_bounded(&zero, 64 * 1024), None);
        assert_eq!(read_truncated(&zero, 16), None);
        assert!(start.elapsed() < Duration::from_secs(5));
        Ok(())
    }
}
