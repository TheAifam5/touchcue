//! Proxy on scdaemon's own socket.
//!
//! In `--multi-server` mode scdaemon also listens on `S.scdaemon` in the
//! gpg socket directory, and gpg-agent connects there whenever the pipe
//! connection is in use. The proxy binds a listener at `S.scdaemon.touchcue`
//! and atomically swaps the two paths with `RENAME_EXCHANGE`, so
//! `S.scdaemon` is always connectable: afterwards clients reach the proxy,
//! which forwards each connection byte for byte to scdaemon's socket, now at
//! `S.scdaemon.touchcue`, and tracks it like the pipe.
//!
//! The swap happens only when the socket directory is private to this user
//! and the socket at `S.scdaemon` is served by the wrapper's own child.
//! Every later path operation checks the expected inode first, so a socket
//! that a later scdaemon bound is left alone, except in the short window
//! between that check and the rename or unlink, which a concurrent rebind
//! could hit.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{CWD, RenameFlags, renameat_with};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::session::{Reporter, Session, Side, pump};

/// File name of scdaemon's socket in the gpg socket directory.
const SOCKET_NAME: &str = "S.scdaemon";
/// Suffix of the path scdaemon's socket is moved to.
const MOVED_SUFFIX: &str = ".touchcue";
/// Connections forwarded at once; more are closed on accept.
const MAX_CONNECTIONS: usize = 32;
/// Longest wait to connect to scdaemon's moved socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Device and inode numbers of a file.
type Inode = (u64, u64);

/// Failure to set up, run or take down the proxy.
#[derive(Debug, thiserror::Error)]
pub(super) enum ProxyError {
    #[error("cannot locate the gpg socket directory")]
    Locate(#[source] crate::gpg::GpgError),
    #[error("{} does not exist", path.display())]
    Missing { path: PathBuf },
    #[error("{} is not a socket of this user", path.display())]
    Unexpected { path: PathBuf },
    #[error("{} is not a directory private to this user", path.display())]
    InsecureDir { path: PathBuf },
    #[error("{} is not served by the scdaemon this wrapper started", path.display())]
    NotOurs { path: PathBuf },
    #[error("{} is in use by another process", path.display())]
    Busy { path: PathBuf },
    #[error("cannot {action} {}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("socket setup task failed")]
    Task(#[source] tokio::task::JoinError),
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ProxyError {
    let path = path.to_owned();
    move |source| ProxyError::Io {
        action,
        path,
        source,
    }
}

/// Paths and inodes after the swap.
#[derive(Debug, Clone)]
struct Swapped {
    /// `S.scdaemon`, now the proxy's listener.
    public: PathBuf,
    proxy: Inode,
    /// `S.scdaemon.touchcue`, now scdaemon's socket.
    moved: PathBuf,
    scdaemon: Inode,
}

/// Returns the value of the last `--homedir <dir>` or `--homedir=<dir>` in
/// scdaemon's arguments.
fn homedir(args: &[OsString]) -> Option<&OsStr> {
    let mut found = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--homedir" {
            found = args.next().map(OsString::as_os_str).or(found);
        } else if let Some(value) = arg.as_bytes().strip_prefix(b"--homedir=") {
            found = Some(OsStr::from_bytes(value));
        }
    }
    found
}

/// Runs the proxy for scdaemon started with `args` until `stop` is
/// cancelled, then takes it down. `scdaemon_running` tells, when `stop` is
/// cancelled, whether scdaemon still runs and keeps its socket.
///
/// Waits for `session`, the pipe connection, to see scdaemon's first output:
/// scdaemon creates its socket before it answers on the pipe. The socket is
/// taken over only when its directory is private to this user and a probe
/// connection shows that `child`, the pid of the scdaemon the wrapper
/// started, serves it.
///
/// # Errors
///
/// Returns [`ProxyError`] when the proxy could not be set up or failed;
/// the pipe connection is unaffected and the paths are restored.
#[tracing::instrument(name = "proxy", skip_all)]
pub(super) async fn run(
    args: Vec<OsString>,
    child: Option<u32>,
    session: &Session,
    reporter: Reporter,
    stop: CancellationToken,
    scdaemon_running: impl Fn() -> bool,
) -> Result<(), ProxyError> {
    tokio::select! {
        () = session.server_output() => {}
        () = stop.cancelled() => return Ok(()),
    }
    let dir = crate::gpg::gpgconf_dir(homedir(&args), "socketdir")
        .await
        .map_err(ProxyError::Locate)?;
    let public = dir.join(SOCKET_NAME);
    let expected = blocking({
        let public = public.clone();
        move || {
            private_dir(&dir)?;
            own_socket(&public)?.ok_or(ProxyError::Missing { path: public })
        }
    })
    .await?;
    served_by(&public, child).await?;
    let (listener, swapped) = blocking(move || swap(public, expected)).await?;
    tracing::debug!(path = %swapped.public.display(), "proxying scdaemon socket");
    let served = serve(listener, &swapped, reporter, &stop).await;
    let restore = scdaemon_running();
    let cleanup = blocking({
        let swapped = swapped.clone();
        move || take_down(&swapped, restore)
    })
    .await;
    match (served, cleanup) {
        (Err(error), Err(cleanup)) => {
            tracing::warn!(
                error = &cleanup as &dyn std::error::Error,
                "cannot take the socket proxy down"
            );
            Err(error)
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ProxyError> + Send + 'static,
) -> Result<T, ProxyError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ProxyError::Task)?
}

/// Requires `dir` to be a directory owned by this user with no group or
/// other permissions.
fn private_dir(dir: &Path) -> Result<(), ProxyError> {
    let meta = fs::symlink_metadata(dir).map_err(io_error("inspect", dir))?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(ProxyError::InsecureDir {
            path: dir.to_owned(),
        });
    }
    Ok(())
}

/// Connects to the socket at `path` and requires the listening process to
/// be `child`.
async fn served_by(path: &Path, child: Option<u32>) -> Result<(), ProxyError> {
    let stream = match timeout(CONNECT_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(source)) => return Err(io_error("probe", path)(source)),
        Err(elapsed) => {
            return Err(io_error("probe", path)(io::Error::new(
                ErrorKind::TimedOut,
                elapsed,
            )));
        }
    };
    let cred = stream
        .peer_cred()
        .map_err(io_error("check the peer of", path))?;
    match (cred.pid().map(i64::from), child.map(i64::from)) {
        (Some(pid), Some(child)) if pid == child => Ok(()),
        _ => Err(ProxyError::NotOurs {
            path: path.to_owned(),
        }),
    }
}

/// Binds the proxy at `<public>.touchcue` and swaps it with scdaemon's
/// socket at `public`, which must still be the inode `scdaemon`.
fn swap(public: PathBuf, scdaemon: Inode) -> Result<(UnixListener, Swapped), ProxyError> {
    if own_socket(&public)? != Some(scdaemon) {
        return Err(ProxyError::Unexpected { path: public });
    }
    let mut moved = public.clone().into_os_string();
    moved.push(MOVED_SUFFIX);
    let moved = PathBuf::from(moved);
    clear_stale(&moved)?;
    let listener = StdListener::bind(&moved).map_err(io_error("bind", &moved))?;
    let proxy = inode(&moved)?;
    let swapped = Swapped {
        public,
        proxy,
        moved,
        scdaemon,
    };
    let prepared = fs::set_permissions(&swapped.moved, fs::Permissions::from_mode(0o600))
        .map_err(io_error("set the mode of", &swapped.moved))
        .and_then(|()| {
            listener
                .set_nonblocking(true)
                .map_err(io_error("configure", &swapped.moved))
        })
        .and_then(|()| {
            exchange(&swapped.moved, &swapped.public)
                .map_err(io_error("swap with", &swapped.public))
        })
        .and_then(|()| {
            UnixListener::from_std(listener).map_err(io_error("listen on", &swapped.public))
        });
    match prepared {
        Ok(listener) => Ok((listener, swapped)),
        Err(error) => {
            if let Err(cleanup) = take_down(&swapped, true) {
                tracing::warn!(
                    error = &cleanup as &dyn std::error::Error,
                    "cannot undo a failed socket swap"
                );
            }
            Err(error)
        }
    }
}

/// Returns the inode of the socket at `path` when it belongs to this user,
/// or `None` when nothing is there.
fn own_socket(path: &Path) -> Result<Option<Inode>, ProxyError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("inspect", path)(source)),
    };
    if !meta.file_type().is_socket() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(ProxyError::Unexpected {
            path: path.to_owned(),
        });
    }
    Ok(Some((meta.dev(), meta.ino())))
}

fn inode(path: &Path) -> Result<Inode, ProxyError> {
    let meta = fs::symlink_metadata(path).map_err(io_error("inspect", path))?;
    Ok((meta.dev(), meta.ino()))
}

/// Removes a socket left at `path` by an earlier proxy that did not clean up,
/// once it refuses connections.
fn clear_stale(path: &Path) -> Result<(), ProxyError> {
    let Some(stale) = own_socket(path)? else {
        return Ok(());
    };
    match StdUnixStream::connect(path) {
        Ok(_stream) => {
            return Err(ProxyError::Busy {
                path: path.to_owned(),
            });
        }
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {}
        Err(source) => return Err(io_error("probe", path)(source)),
    }
    remove_if(path, stale)
}

/// Swaps the files at `a` and `b` atomically.
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE).map_err(io::Error::from)
}

/// Removes `path` if it is still `expected`.
fn remove_if(path: &Path, expected: Inode) -> Result<(), ProxyError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if (meta.dev(), meta.ino()) == expected => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error("inspect", path)(source)),
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove", path)(source)),
    }
}

/// Undoes the swap. With `restore`, scdaemon's socket is moved back to the
/// public path when both paths still hold the expected sockets; then the
/// proxy's socket and, without `restore`, scdaemon's leftover socket are
/// removed.
fn take_down(swapped: &Swapped, restore: bool) -> Result<(), ProxyError> {
    let at = |path: &Path| match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some((meta.dev(), meta.ino()))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("inspect", path)(source)),
    };
    let public = at(&swapped.public)?;
    let moved = at(&swapped.moved)?;
    if restore && public == Some(swapped.proxy) && moved == Some(swapped.scdaemon) {
        exchange(&swapped.moved, &swapped.public).map_err(io_error("restore", &swapped.public))?;
    }
    remove_if(&swapped.public, swapped.proxy)?;
    remove_if(&swapped.moved, swapped.proxy)?;
    if !restore {
        remove_if(&swapped.moved, swapped.scdaemon)?;
    }
    Ok(())
}

/// Accepts connections until `stop` is cancelled or accepting fails.
async fn serve(
    listener: UnixListener,
    swapped: &Swapped,
    reporter: Reporter,
    stop: &CancellationToken,
) -> Result<(), ProxyError> {
    let uid = rustix::process::geteuid().as_raw();
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            () = stop.cancelled() => break Ok(()),
            Some(joined) = connections.join_next() => {
                if let Err(error) = joined {
                    tracing::warn!(error = &error as &dyn std::error::Error, "connection task failed");
                }
            }
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _addr)) => stream,
                    Err(error) if is_transient(&error) => {
                        tracing::debug!(error = &error as &dyn std::error::Error, "accept failed");
                        continue;
                    }
                    Err(source) => break Err(io_error("accept on", &swapped.public)(source)),
                };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == uid => {}
                    Ok(cred) => {
                        tracing::warn!(uid = cred.uid(), "connection from another user refused");
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(error = &error as &dyn std::error::Error, "cannot read peer credentials");
                        continue;
                    }
                }
                if connections.len() >= MAX_CONNECTIONS {
                    tracing::warn!(limit = MAX_CONNECTIONS, "too many connections; closing one");
                    continue;
                }
                connections.spawn(forward(stream, swapped.moved.clone(), reporter.clone()));
            }
        }
    };
    connections.shutdown().await;
    result
}

fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::Interrupted | ErrorKind::WouldBlock
    ) || matches!(
        error.raw_os_error(),
        Some(code) if code == rustix::io::Errno::MFILE.raw_os_error()
            || code == rustix::io::Errno::NFILE.raw_os_error()
            || code == rustix::io::Errno::NOBUFS.raw_os_error()
            || code == rustix::io::Errno::NOMEM.raw_os_error()
    )
}

/// Forwards one client connection to scdaemon's socket at `upstream` and
/// tracks it.
#[tracing::instrument(name = "connection", skip_all)]
async fn forward(client: UnixStream, upstream: PathBuf, reporter: Reporter) {
    let server = match timeout(CONNECT_TIMEOUT, UnixStream::connect(&upstream)).await {
        Ok(Ok(server)) => server,
        Ok(Err(error)) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "cannot reach scdaemon"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "cannot reach scdaemon"
            );
            return;
        }
    };
    let session = Session::new(reporter);
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let both = async {
        tokio::join!(
            pump(client_read, server_write, Side::Client, &session),
            pump(server_read, client_write, Side::Server, &session),
        )
    };
    tokio::select! {
        ((), ()) = both => {}
        () = session.timer() => {}
    }
    session.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn homedir_accepts_both_spellings() {
        assert_eq!(homedir(&args(&["--multi-server"])), None);
        assert_eq!(
            homedir(&args(&["--multi-server", "--homedir", "/a"])),
            Some(OsStr::new("/a"))
        );
        assert_eq!(
            homedir(&args(&["--multi-server", "--homedir=/b"])),
            Some(OsStr::new("/b"))
        );
        assert_eq!(
            homedir(&args(&["--homedir=/a", "--homedir", "/c"])),
            Some(OsStr::new("/c"))
        );
        assert_eq!(homedir(&args(&["--homedir"])), None);
    }
}
