use std::collections::BTreeMap;
use std::fs::{self, DirBuilder};
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use touchcue_core::{EndReason, Event, RequestState, Source};

use crate::client::{MAX_CLIENTS, WRITE_TIMEOUT};
use crate::wire::tests::request;
use crate::{Endpoints, Ipc, IpcConfig, IpcError, WireEvent};

/// Failure of a test step.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TestError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Dbus(#[from] zbus::Error),
    #[error(transparent)]
    Fdo(#[from] zbus::fdo::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    #[error("cannot send to a channel")]
    Send,
    #[error("no semaphore permit")]
    NoPermit(#[from] tokio::sync::TryAcquireError),
    #[error("no socket at the path")]
    NoSocket,
    #[error("expected data did not arrive in time")]
    TimedOut(#[from] tokio::time::error::Elapsed),
}

impl<T> From<tokio::sync::broadcast::error::SendError<T>> for TestError {
    fn from(_unsent: tokio::sync::broadcast::error::SendError<T>) -> TestError {
        TestError::Send
    }
}

type TestResult = Result<(), TestError>;

/// Deadline for any expected bytes; generous because tests share the machine with builds.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Request id used only to wait for client registration.
const SENTINEL: u64 = 999_999;

/// Awaits `future` for at most [`READ_TIMEOUT`].
async fn within<T>(future: impl Future<Output = io::Result<T>>) -> Result<T, TestError> {
    tokio::time::timeout(READ_TIMEOUT, future)
        .await?
        .map_err(TestError::from)
}

/// Creates a private runtime directory, as `$XDG_RUNTIME_DIR` is.
fn runtime_dir() -> io::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn config(runtime_dir: &Path) -> IpcConfig {
    IpcConfig {
        runtime_dir: runtime_dir.to_owned(),
        json: true,
        dbus: false,
        compat_maxbaz: true,
    }
}

fn json_only(runtime_dir: &Path) -> IpcConfig {
    IpcConfig {
        compat_maxbaz: false,
        ..config(runtime_dir)
    }
}

async fn spawn(cfg: IpcConfig) -> Result<Ipc, IpcError> {
    Ipc::spawn(cfg, CancellationToken::new()).await
}

fn json_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("touchcue/events.sock")
}

fn compat_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("yubikey-touch-detector.socket")
}

async fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path).await
}

fn started(id: u64, source: Source) -> WireEvent {
    WireEvent::new(
        &Event::Started(request(id, source, RequestState::Waiting)),
        &BTreeMap::new(),
    )
}

fn updated(id: u64, source: Source, value: &str) -> WireEvent {
    let values = BTreeMap::from([("app.name".to_owned(), value.to_owned())]);
    WireEvent::new(
        &Event::Updated(request(id, source, RequestState::Waiting)),
        &values,
    )
}

fn lingering(id: u64, source: Source) -> WireEvent {
    WireEvent::new(
        &Event::Updated(request(
            id,
            source,
            RequestState::Lingering(EndReason::Touched),
        )),
        &BTreeMap::new(),
    )
}

fn ended(id: u64, source: Source) -> WireEvent {
    WireEvent::new(
        &Event::Ended {
            request: request(id, source, RequestState::Lingering(EndReason::Touched)),
            reason: EndReason::Touched,
        },
        &BTreeMap::new(),
    )
}

async fn read_raw(reader: &mut BufReader<UnixStream>) -> Result<serde_json::Value, TestError> {
    let mut line = String::new();
    within(reader.read_line(&mut line)).await?;
    Ok(serde_json::from_str(&line).map_err(io::Error::from)?)
}

/// Returns the next line that is not about the [`SENTINEL`] request.
async fn read_line(reader: &mut BufReader<UnixStream>) -> Result<serde_json::Value, TestError> {
    loop {
        let value = read_raw(reader).await?;
        if value["id"] != SENTINEL {
            return Ok(value);
        }
    }
}

/// Connects a JSON client and returns once the dispatcher registered it.
///
/// The client sees the sentinel either in its snapshot or as a live event,
/// and both happen only after registration.
async fn json_client(ipc: &Ipc, path: &Path) -> Result<BufReader<UnixStream>, TestError> {
    let mut reader = BufReader::new(connect(path).await?);
    ipc.publish(&started(SENTINEL, Source::Ccid));
    while read_raw(&mut reader).await?["id"] != SENTINEL {}
    Ok(reader)
}

async fn read_n<const N: usize>(stream: &mut UnixStream) -> Result<[u8; N], TestError> {
    let mut buf = [0; N];
    within(stream.read_exact(&mut buf)).await?;
    Ok(buf)
}

/// Reads until EOF, which the server sends by closing the connection.
async fn closed(stream: &mut UnixStream) -> Result<Vec<u8>, TestError> {
    let mut rest = Vec::new();
    within(stream.read_to_end(&mut rest)).await?;
    Ok(rest)
}

#[tokio::test]
async fn json_client_gets_snapshot_then_events() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut early = json_client(&ipc, &json_path(dir.path())).await?;

    ipc.publish(&started(1, Source::Fido));
    ipc.publish(&started(2, Source::Gpg));
    ipc.publish(&updated(1, Source::Fido, "ssh"));
    ipc.publish(&ended(2, Source::Gpg));
    let mut kinds = Vec::new();
    for _ in 0..4 {
        let value = read_line(&mut early).await?;
        kinds.push((value["kind"].clone(), value["id"].clone()));
    }
    assert_eq!(
        kinds,
        [
            ("started".into(), 1.into()),
            ("started".into(), 2.into()),
            ("updated".into(), 1.into()),
            ("ended".into(), 2.into()),
        ]
    );

    // The snapshot comes first, then exactly the events published after it.
    let mut late = BufReader::new(connect(&json_path(dir.path())).await?);
    let snapshot = read_line(&mut late).await?;
    assert_eq!(snapshot["kind"], "started");
    assert_eq!(snapshot["id"], 1);
    assert_eq!(snapshot["values"]["app.name"], "ssh");
    ipc.publish(&ended(1, Source::Fido));
    let next = read_line(&mut late).await?;
    assert_eq!((&next["kind"], &next["id"]), (&"ended".into(), &1.into()));

    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn slow_json_client_is_dropped_without_blocking_others() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut slow = json_client(&ipc, &json_path(dir.path()))
        .await?
        .into_inner();
    let mut fast = json_client(&ipc, &json_path(dir.path())).await?;
    // 300 events of 8 KiB exceed the client buffer plus the socket buffer.
    // Each event is read by the fast client before the next is published, so
    // the bounded publish queue never overflows however slow the machine is.
    let big = "x".repeat(8 * 1024);
    for _ in 0..300 {
        ipc.publish(&updated(1, Source::Fido, &big));
        assert_eq!(
            read_line(&mut fast).await?["values"]["app.name"],
            big.as_str()
        );
    }
    closed(&mut slow).await?;
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn client_beyond_limit_is_refused() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut clients = Vec::new();
    for _ in 0..MAX_CLIENTS {
        clients.push(BufReader::new(connect(&json_path(dir.path())).await?));
    }
    let mut extra = connect(&json_path(dir.path())).await?;
    assert_eq!(closed(&mut extra).await?, b"");

    ipc.publish(&started(9, Source::Fido));
    for client in &mut clients {
        assert_eq!(read_line(client).await?["id"], 9);
    }
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn stale_socket_is_replaced() -> TestResult {
    let dir = runtime_dir()?;
    DirBuilder::new()
        .mode(0o700)
        .create(dir.path().join("touchcue"))?;
    drop(UnixListener::bind(json_path(dir.path()))?);
    drop(UnixListener::bind(compat_path(dir.path()))?);
    let ipc = spawn(config(dir.path())).await?;
    assert_eq!(
        ipc.endpoints(),
        Endpoints {
            json: true,
            compat: true,
            dbus: false
        }
    );
    connect(&json_path(dir.path())).await?;
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn live_json_socket_means_already_running() -> TestResult {
    let dir = runtime_dir()?;
    DirBuilder::new()
        .mode(0o700)
        .create(dir.path().join("touchcue"))?;
    let _other = UnixListener::bind(json_path(dir.path()))?;
    let result = spawn(config(dir.path())).await;
    assert!(matches!(
        result,
        Err(IpcError::AlreadyRunning { endpoint: "json" })
    ));
    // The other instance's socket stays, and no compat socket was created.
    assert!(json_path(dir.path()).exists());
    assert!(!compat_path(dir.path()).exists());
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn json_socket_with_full_backlog_means_already_running() -> TestResult {
    let dir = runtime_dir()?;
    DirBuilder::new()
        .mode(0o700)
        .create(dir.path().join("touchcue"))?;
    let socket = tokio::net::UnixSocket::new_stream()?;
    socket.bind(json_path(dir.path()))?;
    let _other = socket.listen(0)?;
    // The listener never accepts; queue connections until the kernel refuses more.
    let mut queued = Vec::new();
    let full = loop {
        match connect(&json_path(dir.path())).await {
            Ok(stream) if queued.len() < 64 => queued.push(stream),
            Ok(_) => break false,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break true,
            Err(error) => return Err(error.into()),
        }
    };
    assert!(full, "backlog did not fill");
    let result = tokio::time::timeout(READ_TIMEOUT, spawn(config(dir.path()))).await?;
    assert!(matches!(
        result,
        Err(IpcError::AlreadyRunning { endpoint: "json" })
    ));
    assert!(json_path(dir.path()).exists());
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn live_compat_socket_is_skipped() -> TestResult {
    let dir = runtime_dir()?;
    let _other = UnixListener::bind(compat_path(dir.path()))?;
    let ipc = spawn(config(dir.path())).await?;
    assert_eq!(
        ipc.endpoints(),
        Endpoints {
            json: true,
            compat: false,
            dbus: false
        }
    );
    ipc.shutdown().await?;
    // The other instance's socket is left alone.
    assert!(compat_path(dir.path()).exists());
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn regular_file_at_socket_path_is_kept() -> TestResult {
    let dir = runtime_dir()?;
    fs::write(compat_path(dir.path()), b"data")?;
    let ipc = spawn(config(dir.path())).await?;
    assert!(!ipc.endpoints().compat);
    ipc.shutdown().await?;
    assert_eq!(fs::read(compat_path(dir.path()))?, b"data");
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn open_runtime_dir_skips_compat() -> TestResult {
    let dir = runtime_dir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))?;
    let ipc = spawn(config(dir.path())).await?;
    assert!(ipc.endpoints().json);
    assert!(!ipc.endpoints().compat);
    assert!(!compat_path(dir.path()).exists());
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn symlinked_socket_dir_is_an_error() -> TestResult {
    let dir = runtime_dir()?;
    let target = dir.path().join("elsewhere");
    DirBuilder::new().mode(0o700).create(&target)?;
    symlink(&target, dir.path().join("touchcue"))?;
    let result = spawn(json_only(dir.path())).await;
    assert!(matches!(result, Err(IpcError::Insecure { .. })));
    // With another endpoint requested, the JSON socket is skipped instead.
    let ipc = spawn(config(dir.path())).await?;
    assert!(!ipc.endpoints().json);
    assert!(ipc.endpoints().compat);
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn open_socket_dir_is_an_error() -> TestResult {
    let dir = runtime_dir()?;
    let sockets = dir.path().join("touchcue");
    DirBuilder::new().mode(0o700).create(&sockets)?;
    fs::set_permissions(&sockets, fs::Permissions::from_mode(0o755))?;
    let result = spawn(json_only(dir.path())).await;
    assert!(matches!(result, Err(IpcError::Insecure { .. })));
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn hung_up_clients_free_their_slots() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    ipc.publish(&started(1, Source::Fido));
    for _ in 0..40 {
        drop(connect(&json_path(dir.path())).await?);
    }
    // Slots free as each client task notices its hangup, without any event;
    // a refused client sees EOF instead of the snapshot, so retry until the
    // deadline.
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let mut client = BufReader::new(connect(&json_path(dir.path())).await?);
        match read_raw(&mut client).await {
            Ok(value) => {
                assert_eq!(
                    (&value["kind"], &value["id"]),
                    (&"started".into(), &1.into())
                );
                break;
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "client refused, retrying"
                );
            }
            Err(error) => return Err(error),
        }
    }
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn connect_flood_does_not_drop_events() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(json_only(dir.path())).await?;
    let mut reader = json_client(&ipc, &json_path(dir.path())).await?;
    let path = json_path(dir.path());
    let flood = tokio::spawn(async move {
        for _ in 0..100 {
            drop(UnixStream::connect(&path).await?);
        }
        Ok::<_, io::Error>(())
    });
    // Fewer events than a client buffer holds, so the reader cannot lag.
    for id in 1..=60 {
        ipc.publish(&started(id, Source::Fido));
    }
    for id in 1..=60 {
        assert_eq!(read_line(&mut reader).await?["id"], id);
    }
    flood.await??;
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn resync_ends_stale_requests() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut json = json_client(&ipc, &json_path(dir.path())).await?;
    let mut compat = connect(&compat_path(dir.path())).await?;
    ipc.publish(&started(1, Source::Fido));
    assert_eq!(&read_n::<5>(&mut compat).await?, b"U2F_1");
    assert_eq!(read_line(&mut json).await?["id"], 1);

    // The daemon knows only request 2: request 1 ended without its event.
    ipc.resync(&[started(2, Source::Gpg)]);
    let gone = read_line(&mut json).await?;
    assert_eq!((&gone["kind"], &gone["id"]), (&"ended".into(), &1.into()));
    assert_eq!(gone["reason"], serde_json::Value::Null);
    let new = read_line(&mut json).await?;
    assert_eq!((&new["kind"], &new["id"]), (&"started".into(), &2.into()));
    assert_eq!(&read_n::<10>(&mut compat).await?, b"U2F_0GPG_1");

    // An unchanged resync sends nothing; the next line is the next event.
    ipc.resync(&[started(2, Source::Gpg)]);
    ipc.publish(&ended(2, Source::Gpg));
    let next = read_line(&mut json).await?;
    assert_eq!((&next["kind"], &next["id"]), (&"ended".into(), &2.into()));
    ipc.shutdown().await?;
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn sockets_are_private_and_removed_on_shutdown() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mode = |path: PathBuf| fs::symlink_metadata(path).map(|m| m.permissions().mode() & 0o777);
    assert_eq!(mode(json_path(dir.path()))?, 0o600);
    assert_eq!(mode(compat_path(dir.path()))?, 0o600);
    assert_eq!(mode(dir.path().join("touchcue"))?, 0o700);
    ipc.shutdown().await?;
    assert!(!json_path(dir.path()).exists());
    assert!(!compat_path(dir.path()).exists());
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn shutdown_returns_within_bound_with_a_stuck_client() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(json_only(dir.path())).await?;
    let _stuck = json_client(&ipc, &json_path(dir.path())).await?;
    // The client never reads again, so its task blocks in a write; fewer
    // events than the client buffer keep it from being dropped for lag.
    let big = "x".repeat(64 * 1024);
    for _ in 0..16 {
        ipc.publish(&updated(1, Source::Fido, &big));
    }
    let started = Instant::now();
    ipc.shutdown().await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed <= WRITE_TIMEOUT + Duration::from_millis(500),
        "{elapsed:?}"
    );
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn compat_follows_waiting_requests() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut early = connect(&compat_path(dir.path())).await?;

    ipc.publish(&started(1, Source::Fido));
    assert_eq!(&read_n::<5>(&mut early).await?, b"U2F_1");
    ipc.publish(&started(2, Source::Ssh));
    assert_eq!(&read_n::<5>(&mut early).await?, b"GPG_1");
    // Another waiting fido request, or one fido request lingering while
    // another still waits, sends nothing.
    ipc.publish(&started(3, Source::Fido));
    ipc.publish(&lingering(1, Source::Fido));

    let mut late = connect(&compat_path(dir.path())).await?;
    assert_eq!(&read_n::<10>(&mut late).await?, b"U2F_1GPG_1");

    ipc.publish(&lingering(3, Source::Fido));
    ipc.publish(&updated(1, Source::Fido, "ssh"));
    ipc.publish(&ended(1, Source::Fido));
    ipc.publish(&ended(3, Source::Fido));
    ipc.publish(&ended(2, Source::Ssh));
    ipc.publish(&started(4, Source::Hmac));
    ipc.publish(&ended(4, Source::Hmac));
    for client in [&mut early, &mut late] {
        assert_eq!(
            &read_n::<30>(client).await?,
            b"U2F_0U2F_1U2F_0GPG_0MAC_1MAC_0"
        );
    }

    ipc.shutdown().await?;
    // After shutdown the server closed the connection with nothing more sent.
    assert_eq!(closed(&mut early).await?, b"");
    dir.close()?;
    Ok(())
}

#[tokio::test]
async fn cancelling_the_parent_token_stops_without_drain() -> TestResult {
    let dir = runtime_dir()?;
    let parent = CancellationToken::new();
    let ipc = Ipc::spawn(config(dir.path()), parent.clone()).await?;
    let mut client = json_client(&ipc, &json_path(dir.path()))
        .await?
        .into_inner();
    parent.cancel();
    assert_eq!(closed(&mut client).await?, b"");
    let started = Instant::now();
    ipc.shutdown().await?;
    assert!(started.elapsed() < WRITE_TIMEOUT);
    assert!(!json_path(dir.path()).exists());
    assert!(!compat_path(dir.path()).exists());
    dir.close()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_on_a_multi_thread_runtime() -> TestResult {
    let dir = runtime_dir()?;
    let ipc = spawn(config(dir.path())).await?;
    let mut json = json_client(&ipc, &json_path(dir.path())).await?;
    let mut compat = connect(&compat_path(dir.path())).await?;
    ipc.publish(&started(1, Source::Fido));
    assert_eq!(read_line(&mut json).await?["id"], 1);
    assert_eq!(&read_n::<5>(&mut compat).await?, b"U2F_1");
    ipc.shutdown().await?;
    assert_eq!(closed(&mut compat).await?, b"");
    dir.close()?;
    Ok(())
}
