//! Loading of popup icons on the blocking pool, with caches.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, TryAcquireError};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

use resvg::usvg;
use rustix::fs::{FileType, Mode, OFlags};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use touchcue_core::RateLimit;
use touchcue_core::text::sanitize;
use tracing::{Span, debug, error, instrument, warn};

use super::WARN_INTERVAL;

/// Icon edge length in pixels.
pub(crate) const ICON: u32 = 48;
pub(crate) const ICON_F: f32 = 48.0;
/// Longest time the UI task waits for one icon.
pub(crate) const ICON_DEADLINE: Duration = Duration::from_millis(500);
/// Largest icon file read, in bytes.
const MAX_ICON_LEN: usize = 1024 * 1024;
const MAX_ICON_BYTES: u64 = MAX_ICON_LEN as u64;
/// Largest number of XML nodes parsed in an SVG icon.
const MAX_SVG_NODES: u32 = 100_000;
/// Decoded icons kept; the oldest is evicted first.
const CACHE_ENTRIES: usize = 32;
/// Failed or timed-out icons remembered; the oldest is evicted first.
const FAILED_ENTRIES: usize = 32;
/// Time after which a failed icon is tried again.
const FAILED_TTL: Duration = Duration::from_secs(60);
/// Longest icon path recorded in diagnostics, in characters.
const MAX_LOGGED_PATH_CHARS: usize = 256;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Why an icon is not drawn.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IconError {
    #[error("failed to open the icon file")]
    Open(#[from] rustix::io::Errno),
    #[error("icon is not a regular file")]
    NotRegular,
    #[error("failed to read the icon file")]
    Read(#[from] std::io::Error),
    #[error("icon file exceeds {MAX_ICON_BYTES} bytes")]
    TooLarge,
    /// The decoder error is logged where it occurs; `png` is not a direct dependency.
    #[error("invalid PNG icon")]
    Png,
    #[error("SVG icon is not UTF-8")]
    NotUtf8(#[from] std::str::Utf8Error),
    /// Also returned for a DTD or more than [`MAX_SVG_NODES`] nodes.
    #[error("malformed SVG icon")]
    Xml(#[from] usvg::roxmltree::Error),
    #[error("invalid SVG icon")]
    Svg(#[from] usvg::Error),
    #[error("icon has no area")]
    Empty,
}

type Key = (PathBuf, u32);
type Load = fn(&Path) -> Result<Pixmap, IconError>;

/// Decodes icons on the blocking pool, one at a time, waiting at most a
/// deadline per icon.
pub(crate) struct IconLoader {
    load: Load,
    /// Held by the one running decode; a busy loader skips further icons.
    busy: Arc<Semaphore>,
    /// Decode that outlived its deadline, kept to collect its late result.
    late: Option<(PathBuf, JoinHandle<Result<Pixmap, IconError>>)>,
    deadline: Duration,
    cache: VecDeque<(Key, Pixmap)>,
    /// Icons that failed or timed out, with the time they may be retried.
    failed: VecDeque<(Key, Instant)>,
    warn_limit: RateLimit,
}

impl IconLoader {
    /// Creates a loader whose [`IconLoader::get`] waits at most `deadline`.
    pub(crate) fn new(deadline: Duration) -> Self {
        Self::with_load(deadline, load_icon)
    }

    fn with_load(deadline: Duration, load: Load) -> Self {
        Self {
            load,
            busy: Arc::new(Semaphore::new(1)),
            late: None,
            deadline,
            cache: VecDeque::new(),
            failed: VecDeque::new(),
            warn_limit: RateLimit::new(WARN_INTERVAL),
        }
    }

    /// Returns the decoded icon at `path`, or `None` when it cannot be
    /// loaded within the deadline.
    ///
    /// An icon that failed or timed out is not tried again for [`FAILED_TTL`].
    pub(crate) async fn get(&mut self, path: &Path) -> Option<Pixmap> {
        self.get_at(path, Instant::now()).await
    }

    /// Implements [`IconLoader::get`] with `now` as the time for cache expiry.
    #[instrument(level = "debug", skip_all, fields(path = %log_path(path)))]
    async fn get_at(&mut self, path: &Path, now: Instant) -> Option<Pixmap> {
        self.collect_late(now).await;
        if let Some((_, icon)) = self
            .cache
            .iter()
            .find(|((cached, size), _)| cached == path && *size == ICON)
        {
            debug!("icon cache hit");
            return Some(icon.clone());
        }
        self.failed.retain(|(_, retry_at)| *retry_at > now);
        if self
            .failed
            .iter()
            .any(|((failed, size), _)| failed == path && *size == ICON)
        {
            debug!("icon failed recently; skipping it");
            return None;
        }
        let permit = match Arc::clone(&self.busy).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                self.warn_limit
                    .log(std::time::Instant::now(), |suppressed| {
                        warn!(suppressed, "icon loader busy; skipping icon");
                    });
                return None;
            }
            Err(error @ TryAcquireError::Closed) => {
                self.warn_limit
                    .log(std::time::Instant::now(), |suppressed| {
                        error!(
                            error = &error as &dyn std::error::Error,
                            suppressed, "icon loader closed; skipping icon"
                        );
                    });
                return None;
            }
        };
        let load = self.load;
        let owned = path.to_path_buf();
        let span = Span::current();
        let mut decode = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            span.in_scope(|| load(&owned))
        });
        match tokio::time::timeout(self.deadline, &mut decode).await {
            Ok(joined) => return self.store(path.to_path_buf(), joined, now),
            Err(elapsed) => {
                self.warn_limit
                    .log(std::time::Instant::now(), |suppressed| {
                        warn!(
                            error = &elapsed as &dyn std::error::Error,
                            deadline_ms = self.deadline.as_millis(),
                            suppressed,
                            "icon not loaded in time; skipping it"
                        );
                    });
            }
        }
        self.remember_failure(path.to_path_buf(), now);
        self.late = Some((path.to_path_buf(), decode));
        None
    }

    /// Stores the result of a decode that outlived its deadline once it has finished.
    async fn collect_late(&mut self, now: Instant) {
        if !self
            .late
            .as_ref()
            .is_some_and(|(_, decode)| decode.is_finished())
        {
            return;
        }
        if let Some((path, decode)) = self.late.take() {
            let joined = decode.await;
            self.store(path, joined, now);
        }
    }

    /// Caches a decoded icon, replacing a failure recorded for it, and
    /// returns it; a failure is recorded at `now`.
    fn store(
        &mut self,
        path: PathBuf,
        joined: Result<Result<Pixmap, IconError>, JoinError>,
        now: Instant,
    ) -> Option<Pixmap> {
        match joined {
            Ok(Ok(icon)) => {
                self.failed
                    .retain(|((failed, size), _)| !(*failed == path && *size == ICON));
                if self.cache.len() >= CACHE_ENTRIES {
                    self.cache.pop_front();
                }
                self.cache.push_back(((path, ICON), icon.clone()));
                Some(icon)
            }
            Ok(Err(err)) => {
                debug!(
                    path = %log_path(&path),
                    error = &err as &dyn std::error::Error,
                    "skipping popup icon"
                );
                self.remember_failure(path, now);
                None
            }
            Err(err) => {
                warn!(
                    path = %log_path(&path),
                    error = &err as &dyn std::error::Error,
                    "icon decode task failed"
                );
                self.remember_failure(path, now);
                None
            }
        }
    }

    fn remember_failure(&mut self, path: PathBuf, now: Instant) {
        if self.failed.len() >= FAILED_ENTRIES {
            self.failed.pop_front();
        }
        self.failed.push_back(((path, ICON), now + FAILED_TTL));
    }
}

/// Returns `path` with control and invisible characters replaced, capped at
/// [`MAX_LOGGED_PATH_CHARS`] characters, for recording in diagnostics.
fn log_path(path: &Path) -> String {
    sanitize(&path.to_string_lossy(), MAX_LOGGED_PATH_CHARS).unwrap_or_default()
}

/// Reads and decodes a PNG or SVG icon scaled to fit `ICON` pixels.
#[instrument(level = "debug", skip_all, fields(path = %log_path(path)))]
fn load_icon(path: &Path) -> Result<Pixmap, IconError> {
    let data = read_icon(path)?;
    let mut icon = Pixmap::new(ICON, ICON).ok_or(IconError::Empty)?;
    if data.starts_with(PNG_MAGIC) {
        let image = Pixmap::decode_png(&data).map_err(|err| {
            debug!(
                error = &err as &dyn std::error::Error,
                "PNG icon failed to decode"
            );
            IconError::Png
        })?;
        let transform =
            fit(dimension(image.width()), dimension(image.height())).ok_or(IconError::Empty)?;
        icon.draw_pixmap(
            0,
            0,
            image.as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bicubic,
                ..PixmapPaint::default()
            },
            transform,
            None,
        );
    } else {
        let text = std::str::from_utf8(&data)?;
        let xml = usvg::roxmltree::Document::parse_with_options(
            text,
            usvg::roxmltree::ParsingOptions {
                allow_dtd: false,
                nodes_limit: MAX_SVG_NODES,
                ..usvg::roxmltree::ParsingOptions::default()
            },
        )?;
        let tree = usvg::Tree::from_xmltree(&xml, &svg_options())?;
        let size = tree.size();
        let transform = fit(size.width(), size.height()).ok_or(IconError::Empty)?;
        resvg::render(&tree, transform, &mut icon.as_mut());
    }
    Ok(icon)
}

/// Reads at most [`MAX_ICON_LEN`] bytes of a regular file, rejecting a
/// FIFO or device (also behind a symlink) without blocking on it.
fn read_icon(path: &Path) -> Result<Vec<u8>, IconError> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(IconError::NotRegular);
    }
    let mut data = Vec::new();
    File::from(fd)
        .take(MAX_ICON_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() > MAX_ICON_LEN {
        return Err(IconError::TooLarge);
    }
    Ok(data)
}

/// Returns SVG options that load no external files and embed only PNG data.
fn svg_options() -> usvg::Options<'static> {
    usvg::Options {
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, data, _| {
                data.starts_with(PNG_MAGIC)
                    .then_some(usvg::ImageKind::PNG(data))
            }),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    }
}

/// Returns the transform that scales a `width` by `height` image to fit
/// the icon square, centred, keeping its aspect ratio.
fn fit(width: f32, height: f32) -> Option<Transform> {
    let longest = width.max(height);
    if longest <= 0.0 || !longest.is_finite() {
        return None;
    }
    let scale = ICON_F / longest;
    Some(Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        (ICON_F - width * scale) / 2.0,
        (ICON_F - height * scale) / 2.0,
    ))
}

#[expect(
    clippy::cast_precision_loss,
    reason = "image dimensions are far below 2^24, where f32 is exact"
)]
fn dimension(pixels: u32) -> f32 {
    pixels as f32
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Errno(#[from] rustix::io::Errno),
        #[error(transparent)]
        Icon(#[from] IconError),
        #[error("failed to encode the PNG fixture")]
        Encode(#[from] png::EncodingError),
        #[error("failed to create the PNG fixture")]
        Pixmap,
    }

    /// Creates an empty directory unique to this process and test.
    fn scratch(name: &str) -> Result<PathBuf, TestError> {
        let dir = std::env::temp_dir().join(format!("touchcue-ui-{}-{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn write_png(path: &Path) -> Result<(), TestError> {
        let mut pixmap = Pixmap::new(4, 2).ok_or(TestError::Pixmap)?;
        pixmap.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        let png = pixmap.encode_png()?;
        std::fs::write(path, png)?;
        Ok(())
    }

    #[test]
    fn missing_icon_is_an_open_error() {
        assert!(matches!(
            load_icon(Path::new("/nonexistent/touchcue-icon.png")),
            Err(IconError::Open(_))
        ));
    }

    #[test]
    fn svg_external_image_is_not_loaded() -> Result<(), TestError> {
        let dir = scratch("svg-href")?;
        let svg = dir.join("icon.svg");
        std::fs::write(
            &svg,
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><image href="/dev/zero" width="10" height="10"/><image xlink:href="file:///dev/zero" width="10" height="10"/></svg>"#,
        )?;
        let started = Instant::now();
        let loaded = load_icon(&svg);
        assert!(started.elapsed() < Duration::from_secs(1));
        loaded?;
        let options = svg_options();
        let resolver = &options.image_href_resolver;
        assert!((resolver.resolve_string)("/dev/zero", &options).is_none());
        let svg_data = Arc::new(b"<svg/>".to_vec());
        assert!((resolver.resolve_data)("image/svg+xml", svg_data, &options).is_none());
        let png_data = Arc::new(PNG_MAGIC.to_vec());
        assert!((resolver.resolve_data)("image/png", png_data, &options).is_some());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn fifo_is_rejected_without_blocking() -> Result<(), TestError> {
        let dir = scratch("fifo")?;
        let fifo = dir.join("x.png");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::from_raw_mode(0o600))?;
        let started = Instant::now();
        assert!(matches!(load_icon(&fifo), Err(IconError::NotRegular)));
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn symlink_to_png_loads() -> Result<(), TestError> {
        let dir = scratch("symlink")?;
        let target = dir.join("real.png");
        write_png(&target)?;
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&target, &link)?;
        load_icon(&link)?;
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn symlink_to_fifo_is_rejected_without_blocking() -> Result<(), TestError> {
        let dir = scratch("symlink-fifo")?;
        let fifo = dir.join("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::from_raw_mode(0o600))?;
        let link = dir.join("x.png");
        std::os::unix::fs::symlink(&fifo, &link)?;
        let started = Instant::now();
        assert!(matches!(load_icon(&link), Err(IconError::NotRegular)));
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn svg_with_dtd_is_rejected() -> Result<(), TestError> {
        let dir = scratch("dtd")?;
        let svg = dir.join("icon.svg");
        std::fs::write(
            &svg,
            r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text>&b;</text></svg>"#,
        )?;
        assert!(matches!(load_icon(&svg), Err(IconError::Xml(_))));
        let mut loader = IconLoader::new(Duration::from_secs(5));
        assert!(loader.get(&svg).await.is_none());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn oversized_icon_is_rejected() -> Result<(), TestError> {
        let dir = scratch("large")?;
        let large = dir.join("large.png");
        std::fs::write(&large, vec![0_u8; MAX_ICON_LEN + 1])?;
        assert!(matches!(load_icon(&large), Err(IconError::TooLarge)));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cache_serves_repeated_icons() -> Result<(), TestError> {
        let dir = scratch("cache")?;
        let png = dir.join("icon.png");
        write_png(&png)?;
        let mut loader = IconLoader::new(Duration::from_secs(5));
        let first = loader.get(&png).await;
        assert!(first.is_some());
        std::fs::remove_file(&png)?;
        assert_eq!(loader.get(&png).await, first);
        assert!(loader.get(&dir.join("missing.png")).await.is_none());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_icon_is_not_retried_until_expiry() -> Result<(), TestError> {
        let dir = scratch("failed")?;
        let png = dir.join("icon.png");
        let mut loader = IconLoader::new(Duration::from_secs(5));
        let t0 = Instant::now();
        assert!(loader.get_at(&png, t0).await.is_none());
        // The file now decodes, so only the failure record keeps it hidden.
        write_png(&png)?;
        let within = t0 + Duration::from_secs(59);
        assert!(loader.get_at(&png, within).await.is_none());
        assert!(loader.get_at(&png, t0 + FAILED_TTL).await.is_some());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn slow_load(_: &Path) -> Result<Pixmap, IconError> {
        std::thread::sleep(Duration::from_millis(300));
        Pixmap::new(ICON, ICON).ok_or(IconError::Empty)
    }

    #[tokio::test]
    async fn deadline_skips_without_blocking_and_keeps_late_result() {
        let mut loader = IconLoader::with_load(Duration::from_millis(20), slow_load);
        let path = Path::new("/slow.png");
        let started = std::time::Instant::now();
        assert!(loader.get(path).await.is_none());
        assert!(loader.get(path).await.is_none());
        assert!(loader.get(Path::new("/other.png")).await.is_none());
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(loader.failed.len(), 1);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(loader.get(path).await.is_some());
        assert!(loader.failed.is_empty());
    }

    #[test]
    fn failures_are_capped() {
        let mut loader = IconLoader::new(Duration::from_secs(5));
        let t0 = Instant::now();
        for index in 0..=FAILED_ENTRIES {
            loader.remember_failure(PathBuf::from(format!("/missing/{index}.png")), t0);
        }
        assert_eq!(loader.failed.len(), FAILED_ENTRIES);
        assert!(
            loader
                .failed
                .front()
                .is_some_and(|((path, _), _)| path.ends_with("1.png"))
        );
    }

    #[tokio::test]
    async fn cache_evicts_the_oldest_entry() -> Result<(), TestError> {
        let dir = scratch("evict")?;
        let mut loader = IconLoader::new(Duration::from_secs(5));
        for index in 0..=CACHE_ENTRIES {
            let png = dir.join(format!("{index}.png"));
            write_png(&png)?;
            assert!(loader.get(&png).await.is_some());
        }
        assert_eq!(loader.cache.len(), CACHE_ENTRIES);
        assert!(
            loader
                .cache
                .front()
                .is_some_and(|((path, _), _)| path.ends_with("1.png"))
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn fit_centres_and_scales() {
        assert_eq!(
            fit(96.0, 48.0),
            Some(Transform::from_row(0.5, 0.0, 0.0, 0.5, 0.0, 12.0))
        );
        assert_eq!(fit(0.0, 0.0), None);
    }
}
