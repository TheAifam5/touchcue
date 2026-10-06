//! Accept loops and the per-client tasks of the JSON and compat sockets.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument;

use crate::dispatch::{Endpoint, Registration, Subscription};
use crate::limit::LogLimit;

/// Most clients per socket; further connections are closed at once.
pub(crate) const MAX_CLIENTS: usize = 32;
/// Most clients one socket accepts per second; further connections wait in
/// the listen backlog.
const ACCEPT_PER_SECOND: u32 = 50;
/// Length of the accept rate window.
const ACCEPT_WINDOW: Duration = Duration::from_secs(1);
/// Pause after a failed accept, such as when the process is out of file descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Longest time one write to a client may take before the client is dropped.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest read of client input, which is discarded.
const READ_CHUNK: usize = 4096;

/// Accepts clients until `accepting` fires and starts a task per client in
/// `tracker`; the client tasks stop when their feed closes or `cancel` fires.
///
/// A client whose peer uid is not `uid`, one beyond [`MAX_CLIENTS`], or one
/// that finds the registration queue full is closed at once.
pub(crate) async fn accept_loop(
    listener: UnixListener,
    endpoint: Endpoint,
    uid: u32,
    registrations: mpsc::Sender<Registration>,
    tracker: TaskTracker,
    accepting: CancellationToken,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let error_limit = LogLimit::new();
    let refuse_limit = LogLimit::new();
    let peer_limit = LogLimit::new();
    let mut window = Instant::now();
    let mut accepted = 0;
    loop {
        let now = Instant::now();
        if now >= window + ACCEPT_WINDOW {
            window = now;
            accepted = 0;
        }
        if accepted >= ACCEPT_PER_SECOND {
            tokio::select! {
                () = accepting.cancelled() => break,
                () = sleep_until(window + ACCEPT_WINDOW) => continue,
            }
        }
        let accepted_stream = tokio::select! {
            () = accepting.cancelled() => break,
            accepted_stream = listener.accept() => accepted_stream,
        };
        let stream = match accepted_stream {
            Ok((stream, _)) => stream,
            Err(error) => {
                if let Some(suppressed) = error_limit.allow() {
                    tracing::warn!(
                        error = &error as &dyn Error,
                        suppressed,
                        "cannot accept client"
                    );
                }
                tokio::select! {
                    () = accepting.cancelled() => break,
                    () = sleep(ACCEPT_BACKOFF) => continue,
                }
            }
        };
        accepted += 1;
        match stream.peer_cred() {
            Ok(cred) if same_user(cred.uid(), uid) => {}
            Ok(_) => {
                if let Some(suppressed) = peer_limit.allow() {
                    tracing::warn!(suppressed, "client of another user refused");
                }
                continue;
            }
            Err(error) => {
                if let Some(suppressed) = peer_limit.allow() {
                    tracing::warn!(
                        error = &error as &dyn Error,
                        suppressed,
                        "cannot read client credentials, client refused"
                    );
                }
                continue;
            }
        }
        let permit = match Arc::clone(&slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                if let Some(suppressed) = refuse_limit.allow() {
                    tracing::warn!(
                        limit = MAX_CLIENTS,
                        suppressed,
                        "client limit reached, client refused"
                    );
                }
                continue;
            }
            Err(error @ TryAcquireError::Closed) => {
                tracing::error!(
                    error = &error as &dyn Error,
                    "client slots closed, socket stops accepting"
                );
                break;
            }
        };
        let (reply, subscription) = oneshot::channel();
        match registrations.try_send(Registration { endpoint, reply }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if let Some(suppressed) = refuse_limit.allow() {
                    tracing::warn!(suppressed, "client registration queue full, client refused");
                }
                continue;
            }
            Err(TrySendError::Closed(_)) => break,
        }
        let clients = MAX_CLIENTS - slots.available_permits();
        tracing::debug!(clients, "client connected");
        let span = tracing::info_span!("ipc_client", ?endpoint);
        tracker.spawn(serve(stream, subscription, permit, cancel.clone()).instrument(span));
    }
    tracing::debug!("accept loop stopped");
}

/// Reports whether a peer with `peer` uid may connect to a daemon running as `own`.
fn same_user(peer: u32, own: u32) -> bool {
    peer == own
}

/// Writes the snapshot, then every feed message, until the client hangs up,
/// falls [`crate::dispatch::CLIENT_BUFFER`] messages behind, takes longer
/// than [`WRITE_TIMEOUT`] for a write, the feed closes, or `cancel` fires.
///
/// Client input is read and discarded; end of input counts as a hangup, so
/// a client that closes its write side is dropped.
pub(crate) async fn serve(
    stream: UnixStream,
    subscription: oneshot::Receiver<Subscription>,
    _slot: OwnedSemaphorePermit,
    cancel: CancellationToken,
) {
    let subscription = tokio::select! {
        () = cancel.cancelled() => return,
        subscription = subscription => subscription,
    };
    let Subscription { snapshot, mut feed } = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn Error,
                "IPC dispatcher stopped before the registration"
            );
            return;
        }
    };
    let (mut input, mut output) = stream.into_split();
    if !snapshot.is_empty() && !write(&mut output, &snapshot, &cancel).await {
        return;
    }
    let mut discard = [0; READ_CHUNK];
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            message = feed.recv() => match message {
                Ok(chunk) => {
                    if !write(&mut output, &chunk, &cancel).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::info!(skipped, "client too slow, dropped");
                    return;
                }
                Err(RecvError::Closed) => {
                    tracing::debug!("feed closed, client done");
                    return;
                }
            },
            read = input.read(&mut discard) => match read {
                Ok(0) => {
                    tracing::debug!("client hung up");
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(error = &error as &dyn Error, "client read failed");
                    return;
                }
            },
        }
    }
}

/// Writes `bytes` and reports whether the client may stay.
#[tracing::instrument(level = "trace", skip_all, fields(len = bytes.len()))]
async fn write(output: &mut OwnedWriteHalf, bytes: &[u8], cancel: &CancellationToken) -> bool {
    let written = tokio::select! {
        () = cancel.cancelled() => return false,
        written = timeout(WRITE_TIMEOUT, output.write_all(bytes)) => written,
    };
    match written {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::debug!(
                error = &error as &dyn Error,
                "client write failed, client dropped"
            );
            false
        }
        Err(_elapsed) => {
            tracing::info!(
                timeout_ms = WRITE_TIMEOUT.as_millis(),
                "client write timed out, client dropped"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::sync::broadcast;

    use super::*;
    use crate::tests::TestError;

    /// Deadline for the accept loop to act on a connection.
    const DEADLINE: Duration = Duration::from_secs(30);

    fn subscription(
        snapshot: &[u8],
        feed: broadcast::Receiver<Arc<[u8]>>,
    ) -> oneshot::Receiver<Subscription> {
        let (reply, rx) = oneshot::channel();
        let sent = reply.send(Subscription {
            snapshot: snapshot.to_vec(),
            feed,
        });
        assert!(matches!(sent, Ok(())), "receiver is alive");
        rx
    }

    fn permit() -> Result<OwnedSemaphorePermit, TestError> {
        Ok(Arc::new(Semaphore::new(1)).try_acquire_owned()?)
    }

    /// Accept loop under test with its socket and registration queue.
    struct Loop {
        dir: tempfile::TempDir,
        path: std::path::PathBuf,
        registered: mpsc::Receiver<Registration>,
        accepting: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl Loop {
        /// Starts an accept loop that admits clients running as `uid`.
        fn start(uid: u32) -> Result<Loop, TestError> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("client.sock");
            let listener = UnixListener::bind(&path)?;
            let (registrations, registered) = mpsc::channel(1);
            let accepting = CancellationToken::new();
            let task = tokio::spawn(accept_loop(
                listener,
                Endpoint::Json,
                uid,
                registrations,
                TaskTracker::new(),
                accepting.clone(),
                CancellationToken::new(),
            ));
            Ok(Loop {
                dir,
                path,
                registered,
                accepting,
                task,
            })
        }

        async fn stop(self) -> Result<(), TestError> {
            self.accepting.cancel();
            self.task.await?;
            self.dir.close()?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn same_user_client_is_registered() -> Result<(), TestError> {
        let mut accepting = Loop::start(crate::socket::euid())?;
        let _client = UnixStream::connect(&accepting.path).await?;
        let registration = timeout(DEADLINE, accepting.registered.recv()).await?;
        assert!(registration.is_some());
        accepting.stop().await
    }

    #[tokio::test]
    async fn other_user_client_is_refused() -> Result<(), TestError> {
        // The loop expects another uid, so this process's client is a foreign peer.
        let mut accepting = Loop::start(crate::socket::euid() ^ 1)?;
        let mut client = UnixStream::connect(&accepting.path).await?;
        let mut received = Vec::new();
        timeout(DEADLINE, client.read_to_end(&mut received)).await??;
        assert_eq!(received, b"");
        assert!(matches!(
            accepting.registered.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        accepting.stop().await
    }

    #[tokio::test]
    async fn lagged_client_is_dropped() -> Result<(), TestError> {
        let (server, mut client) = UnixStream::pair()?;
        let (tx, rx) = broadcast::channel(crate::dispatch::CLIENT_BUFFER);
        for n in 0..=crate::dispatch::CLIENT_BUFFER {
            tx.send(Arc::from(n.to_string().as_bytes()))?;
        }
        serve(
            server,
            subscription(b"snap", rx),
            permit()?,
            CancellationToken::new(),
        )
        .await;
        // Only the snapshot arrives before the connection closes.
        let mut received = Vec::new();
        client.read_to_end(&mut received).await?;
        assert_eq!(received, b"snap");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn write_timeout_drops_client() -> Result<(), TestError> {
        let (server, _client) = UnixStream::pair()?;
        let (_tx, rx) = broadcast::channel(1);
        // Far more than a socket buffer holds, and the client never reads.
        let snapshot = vec![b'x'; 16 * 1024 * 1024];
        let started = Instant::now();
        serve(
            server,
            subscription(&snapshot, rx),
            permit()?,
            CancellationToken::new(),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= WRITE_TIMEOUT && elapsed < 2 * WRITE_TIMEOUT,
            "{elapsed:?}"
        );
        Ok(())
    }
}
