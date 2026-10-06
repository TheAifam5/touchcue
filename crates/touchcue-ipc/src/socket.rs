//! Private socket files: directory checks, binding and cleanup.

use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::IpcError;

/// Longest wait for an existing socket to answer the liveness probe.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// A socket file this process bound, removed only while it is still the same inode.
#[derive(Debug)]
pub(crate) struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
    removed: bool,
}

impl SocketFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the socket file unless it is gone or was replaced by another file.
    #[tracing::instrument(level = "debug", skip_all, fields(path = %self.path.display()))]
    pub(crate) fn remove(&mut self) -> Result<(), IpcError> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        let removed = match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.dev() == self.dev && meta.ino() == self.ino => {
                fs::remove_file(&self.path)
            }
            Ok(_) => {
                tracing::debug!(path = %self.path.display(), "socket file replaced, left in place");
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        removed.map_err(io("cannot remove socket file", &self.path))
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "cannot remove socket file on drop"
            );
        }
    }
}

/// Creates `dir` with mode 0700, or checks that the existing `dir` is a real
/// directory owned by the effective uid and closed to other users.
#[tracing::instrument(level = "debug", skip_all, fields(dir = %dir.display()))]
pub(crate) fn private_dir(dir: &Path) -> Result<(), IpcError> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(IpcError::Socket {
                context: "cannot create socket directory",
                path: dir.to_owned(),
                source,
            });
        }
    }
    check_private_dir(dir)
}

/// Checks that `dir` is a real directory, not a symlink, owned by the
/// effective uid and with no group or other access.
#[tracing::instrument(level = "debug", skip_all, fields(dir = %dir.display()))]
pub(crate) fn check_private_dir(dir: &Path) -> Result<(), IpcError> {
    let meta = fs::symlink_metadata(dir).map_err(io("cannot inspect socket directory", dir))?;
    let insecure = |context| IpcError::Insecure {
        context,
        path: dir.to_owned(),
    };
    if !meta.file_type().is_dir() {
        return Err(insecure("socket directory is not a directory"));
    }
    if meta.uid() != euid() {
        return Err(insecure("socket directory is owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(insecure("socket directory is accessible by other users"));
    }
    Ok(())
}

/// Binds a nonblocking listener at `path` with mode 0600 after clearing a stale socket.
///
/// The returned [`SocketFile`] removes the socket when dropped.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display(), endpoint))]
pub(crate) async fn bind(
    path: PathBuf,
    endpoint: &'static str,
) -> Result<(UnixListener, SocketFile), IpcError> {
    let existing = blocking({
        let path = path.clone();
        move || existing_socket(&path)
    })
    .await?;
    if let Some(inode) = existing {
        probe(&path, endpoint).await?;
        blocking({
            let path = path.clone();
            move || remove_stale(&path, inode, endpoint)
        })
        .await?;
    }
    blocking(move || bind_new(path)).await
}

/// Runs blocking socket file work off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|source| IpcError::TaskFailed {
            task: "socket setup",
            source,
        })?
}

#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
fn bind_new(path: PathBuf) -> Result<(UnixListener, SocketFile), IpcError> {
    let listener = UnixListener::bind(&path).map_err(io("cannot bind socket", &path))?;
    let meta = fs::symlink_metadata(&path).map_err(io("cannot inspect bound socket", &path))?;
    let file = SocketFile {
        path,
        dev: meta.dev(),
        ino: meta.ino(),
        removed: false,
    };
    fs::set_permissions(&file.path, Permissions::from_mode(0o600))
        .map_err(io("cannot set socket mode", &file.path))?;
    listener
        .set_nonblocking(true)
        .map_err(io("cannot make socket nonblocking", &file.path))?;
    Ok((listener, file))
}

/// Device and inode numbers of a file.
type Inode = (u64, u64);

/// Returns the inode of the socket owned by the effective uid at `path`, or
/// `None` when nothing is there; fails when the path holds anything else.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
fn existing_socket(path: &Path) -> Result<Option<Inode>, IpcError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IpcError::Socket {
                context: "cannot inspect socket path",
                path: path.to_owned(),
                source,
            });
        }
    };
    let insecure = |context| IpcError::Insecure {
        context,
        path: path.to_owned(),
    };
    if !meta.file_type().is_socket() {
        return Err(insecure("socket path exists and is not a socket"));
    }
    if meta.uid() != euid() {
        return Err(insecure("socket is owned by another user"));
    }
    Ok(Some((meta.dev(), meta.ino())))
}

/// Connects to the existing socket at `path` to tell a live instance from a
/// stale file, failing with [`IpcError::AlreadyRunning`] for `endpoint`
/// when it is live.
///
/// A refused connection means stale. An accepted connection, a full listen
/// backlog, or no answer within [`PROBE_TIMEOUT`] means live, so a stalled
/// instance is never replaced.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display(), endpoint))]
async fn probe(path: &Path, endpoint: &'static str) -> Result<(), IpcError> {
    let live = match timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_stream)) => "existing socket answers",
        Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => return Ok(()),
        Ok(Err(error)) if error.kind() == ErrorKind::WouldBlock => "existing socket backlog full",
        Ok(Err(source)) => {
            return Err(IpcError::Socket {
                context: "cannot probe existing socket",
                path: path.to_owned(),
                source,
            });
        }
        Err(_elapsed) => "existing socket probe timed out",
    };
    tracing::debug!(endpoint, path = %path.display(), reason = live, "socket is live");
    Err(IpcError::AlreadyRunning { endpoint })
}

/// Removes the stale socket file at `path` if it is still `inode`.
///
/// A path that now holds another file was rebound by a concurrent instance
/// and fails with [`IpcError::AlreadyRunning`] for `endpoint`.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display(), endpoint))]
fn remove_stale(path: &Path, inode: Inode, endpoint: &'static str) -> Result<(), IpcError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if (meta.dev(), meta.ino()) == inode => {}
        Ok(_) => {
            tracing::debug!(endpoint, path = %path.display(), "socket file replaced, left in place");
            return Err(IpcError::AlreadyRunning { endpoint });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(IpcError::Socket {
                context: "cannot inspect stale socket",
                path: path.to_owned(),
                source,
            });
        }
    }
    tracing::info!(path = %path.display(), "removing stale socket");
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(IpcError::Socket {
            context: "cannot remove stale socket",
            path: path.to_owned(),
            source,
        }),
    }
}

/// Returns the effective uid.
pub(crate) fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn io(context: &'static str, path: &Path) -> impl FnOnce(io::Error) -> IpcError {
    let path = path.to_owned();
    move |source| IpcError::Socket {
        context,
        path,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestError;

    #[test]
    fn replaced_socket_is_not_removed() -> Result<(), TestError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("events.sock");
        let _stale = UnixListener::bind(&path)?;
        let inode = existing_socket(&path)?.ok_or(TestError::NoSocket)?;
        let other = dir.path().join("other.sock");
        let _live = UnixListener::bind(&other)?;
        fs::rename(&other, &path)?;

        let removed = remove_stale(&path, inode, "json");

        assert!(matches!(
            removed,
            Err(IpcError::AlreadyRunning { endpoint: "json" })
        ));
        assert!(fs::symlink_metadata(&path)?.file_type().is_socket());
        Ok(())
    }
}
