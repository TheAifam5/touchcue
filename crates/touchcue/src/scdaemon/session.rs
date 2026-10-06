//! One tracked Assuan connection: byte forwarding, the show-delay timer and
//! best-effort reports to the daemon.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use touchcue_core::Outcome;
use touchcue_core::helper::{Message, Origin};

use super::SHOW_DELAY;
use super::tracker::{Event, Tracker};
use crate::helper_socket::{HelperSocketError, SOCKET_PATH, connect, send};
/// Reports queued for the daemon; more are dropped.
const REPORT_QUEUE: usize = 16;
/// Read buffer of each direction.
const BUFFER: usize = 8 * 1024;

/// Which way bytes flow through a connection.
#[derive(Debug, Clone, Copy)]
pub(super) enum Side {
    /// gpg-agent to scdaemon.
    Client,
    /// scdaemon to gpg-agent.
    Server,
}

/// Queue of reports to the daemon, shared by every tracked connection.
#[derive(Debug, Clone)]
pub(super) struct Reporter {
    queue: mpsc::Sender<Message>,
    next_seq: Arc<AtomicU32>,
}

impl Reporter {
    /// Starts the task that delivers reports to the helper socket under
    /// `$XDG_RUNTIME_DIR`. The task ends once every `Reporter` is dropped
    /// and the queue is drained.
    pub(super) fn spawn() -> (Self, JoinHandle<()>) {
        let (queue, receiver) = mpsc::channel(REPORT_QUEUE);
        let task = tokio::spawn(deliver(socket_path(), receiver));
        let reporter = Self {
            queue,
            next_seq: Arc::new(AtomicU32::new(1)),
        };
        (reporter, task)
    }

    fn next_seq(&self) -> u32 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues `message`, dropping it when the queue is full or closed.
    fn send(&self, message: Message) {
        if let Err(error) = self.queue.try_send(message) {
            tracing::debug!(error = &error as &dyn std::error::Error, "report dropped");
        }
    }
}

/// Tracker and timer of one connection.
#[derive(Debug)]
pub(super) struct Session {
    state: Mutex<State>,
    /// Woken when the deadline changes.
    wake: Notify,
    /// Woken on server output.
    output: Notify,
    reporter: Reporter,
}

#[derive(Debug)]
struct State {
    tracker: Tracker,
    deadline: Option<Instant>,
    /// Sequence number of the reported operation that has not ended.
    shown: Option<u32>,
}

impl Session {
    pub(super) fn new(reporter: Reporter) -> Self {
        Self {
            state: Mutex::new(State {
                tracker: Tracker::new(),
                deadline: None,
                shown: None,
            }),
            wake: Notify::new(),
            output: Notify::new(),
            reporter,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                // Only a panic while holding the lock poisons it; the state
                // is still consistent between whole tracker calls.
                tracing::warn!(error = %poisoned, "tracker lock poisoned; continuing");
                poisoned.into_inner()
            }
        }
    }

    /// Feeds forwarded bytes to the tracker and applies its events.
    pub(super) fn observe(&self, side: Side, bytes: &[u8]) {
        let mut events = Vec::new();
        let mut state = self.lock();
        match side {
            Side::Client => state.tracker.client(bytes, &mut events),
            Side::Server => state.tracker.server(bytes, &mut events),
        }
        let deadline = state.deadline;
        for event in events {
            match event {
                Event::Arm => state.deadline = Some(Instant::now() + SHOW_DELAY),
                Event::Disarm => state.deadline = None,
                Event::End(outcome) => self.end(&mut state, outcome),
            }
        }
        let changed = state.deadline != deadline;
        drop(state);
        if changed {
            self.wake.notify_one();
        }
        if matches!(side, Side::Server) {
            self.output.notify_one();
        }
    }

    /// Waits until scdaemon has written anything since the last call.
    pub(super) async fn server_output(&self) {
        self.output.notified().await;
    }

    /// Ends tracking, reporting an unanswered reported operation as
    /// cancelled.
    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.deadline = None;
        if let Some(outcome) = state.tracker.close() {
            self.end(&mut state, outcome);
        }
    }

    fn end(&self, state: &mut State, outcome: Outcome) {
        if let Some(seq) = state.shown.take() {
            self.reporter.send(Message::End {
                origin: Origin::Scdaemon,
                seq,
                outcome,
            });
        }
    }

    /// Reports the operation when its deadline passed.
    fn fire(&self) {
        let mut state = self.lock();
        if state
            .deadline
            .is_none_or(|deadline| deadline > Instant::now())
        {
            return;
        }
        state.deadline = None;
        if let Some(op) = state.tracker.expire() {
            let seq = self.reporter.next_seq();
            state.shown = Some(seq);
            tracing::debug!(seq, op = op.as_str(), "operation waits");
            self.reporter.send(Message::Start {
                origin: Origin::Scdaemon,
                seq,
                op,
                detail: None,
            });
        }
    }

    /// Waits for each deadline and fires it; never returns.
    pub(super) async fn timer(&self) {
        loop {
            let deadline = self.lock().deadline;
            match deadline {
                Some(deadline) => tokio::select! {
                    () = sleep_until(deadline) => self.fire(),
                    () = self.wake.notified() => {}
                },
                None => self.wake.notified().await,
            }
        }
    }
}

/// Copies `from` to `to` until EOF or an error, feeding every chunk to
/// `session` first, then shuts `to` down.
pub(super) async fn pump<R, W>(mut from: R, mut to: W, side: Side, session: &Session)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0; BUFFER];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    ?side,
                    "read failed"
                );
                break;
            }
        };
        let Some(chunk) = buffer.get(..read) else {
            break;
        };
        session.observe(side, chunk);
        if let Err(error) = to.write_all(chunk).await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                ?side,
                "write failed"
            );
            break;
        }
        if let Err(error) = to.flush().await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                ?side,
                "flush failed"
            );
            break;
        }
    }
    if let Err(error) = to.shutdown().await {
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            ?side,
            "close failed"
        );
    }
}

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(&dir).join(SOCKET_PATH))
}

/// Delivers queued reports to the daemon at `socket` until the queue
/// closes. Connects on demand and drops a report it cannot deliver. A report
/// whose write fails on a reused connection, which the daemon may have
/// closed while idle, is sent once more on a new connection.
async fn deliver(socket: Option<PathBuf>, mut queue: mpsc::Receiver<Message>) {
    let mut stream: Option<UnixStream> = None;
    while let Some(message) = queue.recv().await {
        let Some(path) = socket.as_deref() else {
            continue;
        };
        let reused = stream.is_some();
        if let Err(error) = deliver_one(&mut stream, path, &message).await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                reused,
                "cannot report to the daemon"
            );
            if reused && let Err(error) = deliver_one(&mut stream, path, &message).await {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "report dropped after reconnecting"
                );
            }
        }
    }
}

/// Sends `message` on `stream`, connecting to `path` first when there is no
/// connection. A failed write drops the connection.
async fn deliver_one(
    stream: &mut Option<UnixStream>,
    path: &Path,
    message: &Message,
) -> Result<(), HelperSocketError> {
    let conn = match stream {
        Some(conn) => conn,
        None => stream.insert(connect(path).await?),
    };
    if let Err(error) = send(conn, message).await {
        *stream = None;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::net::UnixListener;
    use touchcue_core::Op;
    use touchcue_core::helper::ParseError;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Parse(#[from] ParseError),
        #[error(transparent)]
        Send(#[from] mpsc::error::SendError<Message>),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
        #[error("no report arrived in time")]
        TimedOut(#[from] tokio::time::error::Elapsed),
    }

    fn start(seq: u32) -> Message {
        Message::Start {
            origin: Origin::Scdaemon,
            seq,
            op: Op::Sign,
            detail: None,
        }
    }

    /// Longest wait for one report.
    const WAIT: Duration = Duration::from_secs(5);

    async fn read(listener: &UnixListener) -> Result<(Message, BufReader<UnixStream>), TestError> {
        let (stream, _addr) = tokio::time::timeout(WAIT, listener.accept()).await??;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(WAIT, reader.read_line(&mut line)).await??;
        Ok((Message::parse(&line)?, reader))
    }

    #[tokio::test]
    async fn report_after_an_idle_close_is_retried() -> Result<(), TestError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("helper.sock");
        let listener = UnixListener::bind(&path)?;
        let (queue, receiver) = mpsc::channel(REPORT_QUEUE);
        let task = tokio::spawn(deliver(Some(path), receiver));

        queue.send(start(1)).await?;
        let (first, connection) = read(&listener).await?;
        assert_eq!(first, start(1));
        // The daemon closes the idle connection.
        drop(connection);

        queue.send(start(2)).await?;
        let (second, _connection) = read(&listener).await?;
        assert_eq!(second, start(2));

        drop(queue);
        task.await?;
        Ok(())
    }
}
