//! Established connections of listening unix sockets, read through
//! `NETLINK_SOCK_DIAG`.
//!
//! An accepted unix socket carries the address its listener was bound to,
//! and the kernel reports the inode of its peer, the client end. procfs
//! exposes neither link, so this is how the clients of a server such as
//! gpg-agent are found.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::Duration;

use rustix::net::netlink::{self, SocketAddrNetlink};
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, sockopt};

/// Longest wait for one netlink reply datagram.
const RECV_TIMEOUT: Duration = Duration::from_secs(1);
/// Size of the reply buffer; the kernel fills at most one page per dump datagram.
const RECV_BUFFER: usize = 64 * 1024;
/// Most datagrams read from one dump before it is abandoned.
const MAX_DATAGRAMS: usize = 4096;
/// Most matching connections returned.
const MAX_CONNECTIONS: usize = 4096;

const NLMSG_HDRLEN: usize = 16;
/// Sequence number of the dump request; replies with another one are ignored.
const SEQ: u32 = 1;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const AF_UNIX: u8 = 1;
/// `TCP_ESTABLISHED`, which unix sockets use for connected stream sockets.
const STATE_ESTABLISHED: u32 = 1;
const UDIAG_SHOW_NAME: u32 = 0x1;
const UDIAG_SHOW_PEER: u32 = 0x4;
const UNIX_DIAG_MSG_LEN: usize = 16;
const UNIX_DIAG_NAME: u16 = 0;
const UNIX_DIAG_PEER: u16 = 2;

/// Failure to read the socket table.
#[derive(Debug, thiserror::Error)]
pub enum SockDiagError {
    #[error("{context}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("sock_diag request failed with errno {errno}")]
    Kernel { errno: i32 },
    #[error("malformed sock_diag reply")]
    Malformed,
    #[error("sock_diag message length out of range")]
    Length(#[from] std::num::TryFromIntError),
    #[error("sock_diag dump exceeded {MAX_DATAGRAMS} datagrams")]
    TooLong,
}

/// An established connection accepted on a listening socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Index into the `listeners` passed to [`connections`].
    pub listener: usize,
    /// Inode of the accepted, server-side socket.
    pub inode: u32,
    /// Inode of the client socket.
    pub peer: u32,
}

/// Returns the established connections whose server end is bound to one of
/// `listeners`, at most 4096.
///
/// Blocks for up to one second per reply datagram; call it off the async
/// workers. Sockets of every user are listed; mapping peer inodes to
/// processes is limited to what procfs lets the caller read.
///
/// # Errors
///
/// Returns [`SockDiagError`] when the netlink socket cannot be used, the
/// kernel rejects the request, for example without unix socket diagnostics
/// support, or a reply does not parse.
#[tracing::instrument(level = "debug", skip_all, err)]
pub fn connections(listeners: &[&Path]) -> Result<Vec<Connection>, SockDiagError> {
    let fd = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )
    .map_err(io("cannot open sock_diag socket"))?;
    sockopt::set_socket_timeout(&fd, sockopt::Timeout::Recv, Some(RECV_TIMEOUT))
        .map_err(io("cannot set sock_diag timeout"))?;
    rustix::net::sendto(
        &fd,
        &request(),
        SendFlags::empty(),
        &SocketAddrNetlink::new(0, 0),
    )
    .map_err(io("cannot send sock_diag request"))?;
    let mut buffer = vec![0; RECV_BUFFER];
    let mut found = Vec::new();
    for _ in 0..MAX_DATAGRAMS {
        let (len, full, sender) = rustix::net::recvfrom(&fd, &mut buffer[..], RecvFlags::TRUNC)
            .map_err(io("cannot read sock_diag reply"))?;
        if full > len {
            return Err(SockDiagError::Malformed);
        }
        if !from_kernel(sender) {
            continue;
        }
        let datagram = buffer.get(..len).ok_or(SockDiagError::Malformed)?;
        if parse(datagram, listeners, &mut found)? {
            return Ok(found);
        }
    }
    Err(SockDiagError::TooLong)
}

/// Reports whether a datagram was sent by the kernel, netlink port 0.
fn from_kernel(sender: Option<rustix::net::SocketAddrAny>) -> bool {
    let Some(sender) = sender else {
        tracing::trace!("sock_diag datagram without sender dropped");
        return false;
    };
    match SocketAddrNetlink::try_from(sender) {
        Ok(sender) if sender.pid() == 0 => true,
        Ok(sender) => {
            tracing::trace!(
                port = sender.pid(),
                "sock_diag datagram from a process dropped"
            );
            false
        }
        Err(errno) => {
            let error = std::io::Error::from(errno);
            tracing::trace!(
                error = &error as &dyn std::error::Error,
                "sock_diag datagram sender unreadable"
            );
            false
        }
    }
}

/// Builds a dump request for every established unix socket with its name and peer.
fn request() -> Vec<u8> {
    let len: u32 = 40;
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    out.extend_from_slice(&SEQ.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    // struct unix_diag_req
    out.extend_from_slice(&[AF_UNIX, 0, 0, 0]);
    out.extend_from_slice(&(1u32 << STATE_ESTABLISHED).to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    out.extend_from_slice(&(UDIAG_SHOW_NAME | UDIAG_SHOW_PEER).to_ne_bytes());
    out.extend_from_slice(&[0; 8]);
    out
}

/// Appends the matching connections in one reply datagram to `found` and
/// reports whether the dump is complete.
fn parse(
    mut datagram: &[u8],
    listeners: &[&Path],
    found: &mut Vec<Connection>,
) -> Result<bool, SockDiagError> {
    while datagram.len() >= NLMSG_HDRLEN {
        let len = usize::try_from(u32_at(datagram, 0)?)?;
        let kind = u16_at(datagram, 4)?;
        if len < NLMSG_HDRLEN {
            return Err(SockDiagError::Malformed);
        }
        let message = datagram
            .get(NLMSG_HDRLEN..len)
            .ok_or(SockDiagError::Malformed)?;
        let seq = u32_at(datagram, 8)?;
        let next = datagram.get(align(len)..).unwrap_or_default();
        if seq != SEQ {
            tracing::trace!(seq, "sock_diag message of another request dropped");
            datagram = next;
            continue;
        }
        match kind {
            NLMSG_DONE => return Ok(true),
            NLMSG_ERROR => {
                let errno = i32::from_ne_bytes(array(message, 0)?);
                return Err(SockDiagError::Kernel {
                    errno: errno.saturating_neg(),
                });
            }
            SOCK_DIAG_BY_FAMILY => {
                if let Some(connection) = connection(message, listeners)?
                    && found.len() < MAX_CONNECTIONS
                {
                    found.push(connection);
                }
            }
            _ => {}
        }
        datagram = next;
    }
    Ok(false)
}

/// Parses one `unix_diag_msg` and its attributes.
fn connection(message: &[u8], listeners: &[&Path]) -> Result<Option<Connection>, SockDiagError> {
    let inode = u32_at(message, 4)?;
    let mut attrs = message
        .get(UNIX_DIAG_MSG_LEN..)
        .ok_or(SockDiagError::Malformed)?;
    let mut name = None;
    let mut peer = None;
    while attrs.len() >= 4 {
        let len = usize::from(u16_at(attrs, 0)?);
        let kind = u16_at(attrs, 2)?;
        let data = attrs.get(4..len).ok_or(SockDiagError::Malformed)?;
        match kind {
            UNIX_DIAG_NAME => name = Some(data),
            UNIX_DIAG_PEER => peer = Some(u32_at(data, 0)?),
            _ => {}
        }
        attrs = attrs.get(align(len)..).unwrap_or_default();
    }
    let (Some(name), Some(peer)) = (name, peer) else {
        return Ok(None);
    };
    // A path name carries its terminating NUL; an abstract name starts with one.
    let name = OsStr::from_bytes(name.strip_suffix(b"\0").unwrap_or(name));
    Ok(listeners
        .iter()
        .position(|path| path.as_os_str() == name)
        .map(|listener| Connection {
            listener,
            inode,
            peer,
        }))
}

fn align(len: usize) -> usize {
    len.saturating_add(3) & !3
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], SockDiagError> {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or(SockDiagError::Malformed)
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, SockDiagError> {
    array(bytes, at).map(u16::from_ne_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, SockDiagError> {
    array(bytes, at).map(u32::from_ne_bytes)
}

fn io(context: &'static str) -> impl FnOnce(rustix::io::Errno) -> SockDiagError {
    move |errno| SockDiagError::Io {
        context,
        source: errno.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        SockDiag(#[from] SockDiagError),
        #[error(transparent)]
        Length(#[from] std::num::TryFromIntError),
        #[error("unexpected connections: {0}")]
        Unexpected(String),
    }

    fn attr(kind: u16, data: &[u8]) -> Result<Vec<u8>, TestError> {
        let len = u16::try_from(4 + data.len())?;
        let mut out = Vec::new();
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(data);
        out.resize(align(out.len()), 0);
        Ok(out)
    }

    fn message(kind: u16, body: &[u8]) -> Result<Vec<u8>, TestError> {
        message_seq(kind, SEQ, body)
    }

    fn message_seq(kind: u16, seq: u32, body: &[u8]) -> Result<Vec<u8>, TestError> {
        let len = u32::try_from(NLMSG_HDRLEN + body.len())?;
        let mut out = Vec::new();
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(&[0; 2]);
        out.extend_from_slice(&seq.to_ne_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(body);
        out.resize(align(out.len()), 0);
        Ok(out)
    }

    fn diag(inode: u32, name: &[u8], peer: u32) -> Result<Vec<u8>, TestError> {
        let mut body = vec![AF_UNIX, 1, 1, 0];
        body.extend_from_slice(&inode.to_ne_bytes());
        body.extend_from_slice(&[0; 8]);
        body.extend(attr(UNIX_DIAG_NAME, name)?);
        body.extend(attr(UNIX_DIAG_PEER, &peer.to_ne_bytes())?);
        message(SOCK_DIAG_BY_FAMILY, &body)
    }

    #[test]
    fn parses_matching_connections_until_done() -> Result<(), TestError> {
        let agent = Path::new("/run/user/1000/gnupg/S.gpg-agent");
        let ssh = Path::new("/run/user/1000/gnupg/S.gpg-agent.ssh");
        let mut named = agent.as_os_str().as_bytes().to_vec();
        named.push(0);
        let mut datagram = diag(10, &named, 11)?;
        datagram.extend(diag(12, b"/other.sock", 13)?);
        datagram.extend(diag(14, ssh.as_os_str().as_bytes(), 15)?);
        let mut found = Vec::new();
        assert!(!parse(&datagram, &[agent, ssh], &mut found)?);
        assert!(parse(&message(NLMSG_DONE, &[0; 4])?, &[agent], &mut found)?);
        assert_eq!(
            found,
            [
                Connection {
                    listener: 0,
                    inode: 10,
                    peer: 11
                },
                Connection {
                    listener: 1,
                    inode: 14,
                    peer: 15
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn messages_of_another_request_are_dropped() -> Result<(), TestError> {
        let stray = message_seq(NLMSG_ERROR, SEQ + 1, &(-2i32).to_ne_bytes())?;
        let mut found = Vec::new();
        assert!(!parse(&stray, &[], &mut found)?);
        Ok(())
    }

    #[test]
    fn reports_kernel_errors_and_truncation() -> Result<(), TestError> {
        let error = message(NLMSG_ERROR, &(-2i32).to_ne_bytes())?;
        assert!(matches!(
            parse(&error, &[], &mut Vec::new()),
            Err(SockDiagError::Kernel { errno: 2 })
        ));
        let mut cut = diag(10, b"/a", 11)?;
        cut.truncate(30);
        assert!(matches!(
            parse(&cut, &[Path::new("/a")], &mut Vec::new()),
            Err(SockDiagError::Malformed)
        ));
        Ok(())
    }

    #[test]
    fn finds_the_client_inode_of_a_live_connection() -> Result<(), TestError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path)?;
        let client = UnixStream::connect(&path)?;
        let (_server, _) = listener.accept()?;
        let client_inode = std::fs::metadata(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&client)
        ))?
        .ino();

        let found = connections(&[&path])?;

        let [connection] = found.as_slice() else {
            return Err(TestError::Unexpected(format!("{found:?}")));
        };
        assert_eq!(u64::from(connection.peer), client_inode);
        Ok(())
    }
}
