//! Click-through override-redirect popups on an X11 display.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, IoSlice};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use tiny_skia::Pixmap;
use tokio::io::unix::AsyncFd;
use touchcue_core::config::Position;
use touchcue_core::{RateLimit, RequestId};
use tracing::{debug, instrument, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::properties::WmHints;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, Gcontext, ImageFormat, ImageOrder, PropMode, Rectangle, Visualid, Window,
    WindowClass,
};
use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::reexports::x11rb_protocol::xauth::get_auth;
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;
use x11rb::wrapper::ConnectionExt as _;

use super::WARN_INTERVAL;
use super::icon::{ICON_DEADLINE, IconLoader};
use super::render::{FontState, copy_to_argb8888, render};
use crate::Prompt;
use crate::text::display_body;

/// Distance from the monitor edges, in logical pixels.
const MARGIN: f32 = 16.0;
/// Vertical space between stacked popups, in logical pixels.
const SPACING: f32 = 8.0;
/// Size of a `PutImage` request without its data, in bytes.
const PUT_IMAGE_HEADER: usize = 24;
/// Depth of popup windows, drawn as 32-bit `ZPixmap` pixels.
const DEPTH: u8 = 24;
/// Dots per inch at a scale of 1.
const BASE_DPI: f64 = 96.0;
/// Largest scale applied from `Xft.dpi`.
const MAX_SCALE: f64 = 4.0;
/// Longest wait for the X11 socket to become readable or writable.
const X11_IO_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest `RESOURCE_MANAGER` prefix read, in 32-bit units.
const MAX_RESOURCE_WORDS: u32 = 4096;
/// Longest `RESOURCE_MANAGER` prefix scanned, in bytes.
const MAX_RESOURCE_BYTES: usize = 4 * 4096;
const WM_CLASS: &[u8] = b"touchcue\0touchcue\0";
const NAME: &[u8] = b"touchcue";

/// Why X11 popups are unavailable or stopped working.
#[derive(Debug, thiserror::Error)]
pub(crate) enum X11Error {
    #[error("no X11 display")]
    Connect(#[from] ConnectError),
    #[error("X11 connection failed")]
    Connection(#[from] ConnectionError),
    #[error("X11 request failed")]
    Reply(#[from] ReplyError),
    #[error("X11 request failed")]
    ReplyOrId(#[from] ReplyOrIdError),
    #[error("the X11 server does not use LSB-first image byte order")]
    ByteOrder,
    #[error("the X11 server has no 32-bit pixel format for depth 24")]
    PixelFormat,
    #[error("the X11 root window has depth {0}, not 24")]
    RootDepth(u8),
    #[error("the X11 display has no screen {0}")]
    NoScreen(usize),
    #[error("the X11 server returned the wrong number of atoms")]
    AtomCount,
    #[error("the X11 server has no SHAPE extension")]
    NoShape,
    #[error("failed to register the X11 socket")]
    Register(#[from] io::Error),
}

/// Display servers that may show popups, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayServer {
    Wayland,
    X11,
}

/// Returns the display servers named by the session environment: Wayland
/// when `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` is set, then X11 when
/// `DISPLAY` is set. Empty values count as unset.
pub(crate) fn display_servers(
    wayland_display: Option<&OsStr>,
    wayland_socket: Option<&OsStr>,
    display: Option<&OsStr>,
) -> Vec<DisplayServer> {
    let set = |value: Option<&OsStr>| value.is_some_and(|value| !value.is_empty());
    let mut servers = Vec::new();
    if set(wayland_display) || set(wayland_socket) {
        servers.push(DisplayServer::Wayland);
    }
    if set(display) {
        servers.push(DisplayServer::X11);
    }
    servers
}

/// Returns the last `Xft.dpi` value in a `RESOURCE_MANAGER` property, or
/// `None` when it is missing or not a number.
///
/// Only the first [`MAX_RESOURCE_BYTES`] bytes are scanned. Every other
/// line, including `#include` directives, is ignored.
pub(crate) fn dpi_from_property(property: &[u8]) -> Option<f64> {
    let property = property.get(..MAX_RESOURCE_BYTES).unwrap_or(property);
    let value = property.rsplit(|byte| *byte == b'\n').find_map(|line| {
        let colon = line.iter().position(|byte| *byte == b':')?;
        let (key, value) = line.split_at(colon);
        (key.trim_ascii() == b"Xft.dpi").then(|| value.get(1..).unwrap_or_default())
    })?;
    let text = match std::str::from_utf8(value.trim_ascii()) {
        Ok(text) => text,
        Err(error) => {
            debug!(
                error = &error as &dyn std::error::Error,
                "Xft.dpi is not UTF-8"
            );
            return None;
        }
    };
    match text.parse() {
        Ok(dpi) => Some(dpi),
        Err(error) => {
            debug!(
                error = &error as &dyn std::error::Error,
                "Xft.dpi unparsable"
            );
            None
        }
    }
}

/// Returns the device pixels per logical pixel for an `Xft.dpi` value:
/// 1 when it is missing or invalid, at most [`MAX_SCALE`].
#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is clamped to 1..=4 first"
)]
pub(crate) fn scale_from_dpi(dpi: Option<f64>) -> f32 {
    dpi.filter(|dpi| dpi.is_finite() && *dpi > 0.0)
        .map_or(1.0, |dpi| (dpi / BASE_DPI).clamp(1.0, MAX_SCALE)) as f32
}

/// Returns how many rows of `stride` bytes fit in one `PutImage` request
/// of at most `max_bytes`, or `None` when not even one row fits.
pub(crate) fn rows_per_request(max_bytes: usize, stride: usize) -> Option<usize> {
    let rows = max_bytes
        .checked_sub(PUT_IMAGE_HEADER)?
        .checked_div(stride)?;
    (rows > 0).then_some(rows)
}

/// A monitor rectangle in root window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) primary: bool,
}

impl Monitor {
    fn contains(&self, (x, y): (i32, i32)) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// Returns the monitor under `pointer`, else the primary one, else the first.
pub(crate) fn pick_monitor(monitors: &[Monitor], pointer: (i32, i32)) -> Option<Monitor> {
    monitors
        .iter()
        .find(|monitor| monitor.contains(pointer))
        .or_else(|| monitors.iter().find(|monitor| monitor.primary))
        .or_else(|| monitors.first())
        .copied()
}

/// Returns the top-left corner of a `size` popup on `monitor`, `offset`
/// pixels from the anchored vertical edge and `margin` from the anchored
/// horizontal edge, clamped to the X11 coordinate range.
pub(crate) fn place(
    monitor: Monitor,
    position: Position,
    (width, height): (i32, i32),
    offset: i32,
    margin: i32,
) -> (i16, i16) {
    let left = monitor.x.saturating_add(margin);
    let right = monitor
        .x
        .saturating_add(monitor.width)
        .saturating_sub(width)
        .saturating_sub(margin);
    let centre = monitor
        .x
        .saturating_add(monitor.width.saturating_sub(width) / 2);
    let top = monitor.y.saturating_add(offset);
    let bottom = monitor
        .y
        .saturating_add(monitor.height)
        .saturating_sub(height)
        .saturating_sub(offset);
    let (x, y) = match position {
        Position::TopLeft => (left, top),
        Position::TopRight => (right, top),
        Position::Top => (centre, top),
        Position::BottomLeft => (left, bottom),
        Position::BottomRight => (right, bottom),
        Position::Bottom => (centre, bottom),
    };
    (clamp_i16(x), clamp_i16(y))
}

/// Converts `value` to `i16`, saturating at the type's bounds.
fn clamp_i16(value: i32) -> i16 {
    match i16::try_from(value) {
        Ok(value) => value,
        Err(_) if value < 0 => i16::MIN,
        Err(_) => i16::MAX,
    }
}

/// Returns the rectangles covering the pixels of `pixmap` with alpha of at
/// least half, merging rows with the same horizontal extent.
pub(crate) fn opaque_rows(pixmap: &Pixmap) -> Vec<Rectangle> {
    let width = pixmap.width();
    let mut rects: Vec<Rectangle> = Vec::new();
    let width = match usize::try_from(width) {
        Ok(width) => width,
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "pixmap width out of range; no shape"
            );
            return rects;
        }
    };
    let rows = pixmap.pixels().chunks(width);
    for (y, row) in (0_i16..).zip(rows) {
        let opaque = row.iter().map(|pixel| pixel.alpha() >= 128);
        let Some(first) = opaque.clone().position(|is| is) else {
            continue;
        };
        let last = row.len() - 1 - opaque.rev().position(|is| is).unwrap_or(0);
        let (Ok(x), Ok(span)) = (i16::try_from(first), u16::try_from(last - first + 1)) else {
            continue;
        };
        match rects.last_mut() {
            Some(rect)
                if rect.x == x
                    && rect.width == span
                    && i32::from(rect.y) + i32::from(rect.height) == i32::from(y) =>
            {
                rect.height += 1;
            }
            _ => rects.push(Rectangle {
                x,
                y,
                width: span,
                height: 1,
            }),
        }
    }
    rects
}

/// Atoms set on popup windows.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    window_type: u32,
    type_notification: u32,
    type_utility: u32,
    state: u32,
    state_above: u32,
    state_skip_taskbar: u32,
    state_skip_pager: u32,
    user_time: u32,
    name: u32,
    pid: u32,
    utf8: u32,
}

impl Atoms {
    const NAMES: [&[u8]; 11] = [
        b"_NET_WM_WINDOW_TYPE",
        b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
        b"_NET_WM_WINDOW_TYPE_UTILITY",
        b"_NET_WM_STATE",
        b"_NET_WM_STATE_ABOVE",
        b"_NET_WM_STATE_SKIP_TASKBAR",
        b"_NET_WM_STATE_SKIP_PAGER",
        b"_NET_WM_USER_TIME",
        b"_NET_WM_NAME",
        b"_NET_WM_PID",
        b"UTF8_STRING",
    ];

    /// Interns every atom, sending all requests before reading the replies.
    fn intern(conn: &RustConnection<TimedStream>) -> Result<Self, X11Error> {
        let cookies = Self::NAMES
            .iter()
            .map(|name| conn.intern_atom(false, name))
            .collect::<Result<Vec<_>, _>>()?;
        let atoms = cookies
            .into_iter()
            .map(|cookie| cookie.reply().map(|reply| reply.atom))
            .collect::<Result<Vec<_>, _>>()?;
        let [
            window_type,
            type_notification,
            type_utility,
            state,
            state_above,
            state_skip_taskbar,
            state_skip_pager,
            user_time,
            name,
            pid,
            utf8,
        ] = atoms[..]
        else {
            return Err(X11Error::AtomCount);
        };
        Ok(Self {
            window_type,
            type_notification,
            type_utility,
            state,
            state_above,
            state_skip_taskbar,
            state_skip_pager,
            user_time,
            name,
            pid,
            utf8,
        })
    }
}

/// An X11 connection checked to support popups.
pub(crate) struct Connected {
    conn: RustConnection<TimedStream>,
    root: Window,
    visual: Visualid,
    root_size: (i32, i32),
    max_bytes: usize,
    atoms: Atoms,
    randr: bool,
}

/// The X11 socket; waits on it fail with [`io::ErrorKind::TimedOut`]
/// after [`X11_IO_TIMEOUT`].
#[derive(Debug)]
pub(crate) struct TimedStream(DefaultStream);

impl AsFd for TimedStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for TimedStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Stream for TimedStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::IN;
        }
        if mode.writable() {
            flags |= PollFlags::OUT;
        }
        let mut fds = [PollFd::new(&self.0, flags)];
        let deadline = Instant::now() + X11_IO_TIMEOUT;
        loop {
            let timeout = Timespec::try_from(deadline.saturating_duration_since(Instant::now()))
                .map_err(io::Error::other)?;
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the X11 server did not respond in time",
                    ));
                }
                Ok(_) => return Ok(()),
                Err(Errno::INTR) => {}
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn read(&self, buf: &mut [u8], fd_storage: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.0.read(buf, fd_storage)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.0.write(buf, fds)
    }

    fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.0.write_vectored(bufs, fds)
    }
}

/// Connects to the display named by `$DISPLAY`, trying each of its
/// addresses in order; returns the connection and the screen number.
fn open_display() -> Result<(RustConnection<TimedStream>, usize), ConnectError> {
    let display = parse_display(None)?;
    let screen = usize::from(display.screen);
    let mut last_err = None;
    for address in display.connect_instruction() {
        match DefaultStream::connect(&address) {
            Ok((stream, (family, peer))) => {
                let (auth_name, auth_data) = match get_auth(family, &peer, display.display) {
                    Ok(auth) => auth.unwrap_or_default(),
                    Err(err) => {
                        debug!(
                            error = &err as &dyn std::error::Error,
                            "X11 authority unreadable; connecting without it"
                        );
                        (Vec::new(), Vec::new())
                    }
                };
                return Ok((handshake(stream, screen, auth_name, auth_data)?, screen));
            }
            Err(err) => {
                debug!(
                    error = &err as &dyn std::error::Error,
                    "X11 display address unreachable"
                );
                last_err = Some(err);
            }
        }
    }
    Err(last_err.map_or_else(
        || x11rb::errors::DisplayParsingError::Unknown.into(),
        ConnectError::IoError,
    ))
}

/// Sends the connection setup on `stream` and reads the server's reply,
/// waiting at most [`X11_IO_TIMEOUT`] for each socket operation.
fn handshake(
    stream: DefaultStream,
    screen: usize,
    auth_name: Vec<u8>,
    auth_data: Vec<u8>,
) -> Result<RustConnection<TimedStream>, ConnectError> {
    RustConnection::connect_to_stream_with_auth_info(
        TimedStream(stream),
        screen,
        auth_name,
        auth_data,
    )
}

/// Connects to `$DISPLAY` and checks that popups can be drawn; blocks on
/// the server's replies, each for at most [`X11_IO_TIMEOUT`].
#[instrument(skip_all)]
pub(crate) fn connect() -> Result<Connected, X11Error> {
    let (conn, screen_num) = open_display()?;
    let setup = conn.setup();
    if setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(X11Error::ByteOrder);
    }
    if !setup
        .pixmap_formats
        .iter()
        .any(|format| format.depth == DEPTH && format.bits_per_pixel == 32)
    {
        return Err(X11Error::PixelFormat);
    }
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or(X11Error::NoScreen(screen_num))?;
    if screen.root_depth != DEPTH {
        return Err(X11Error::RootDepth(screen.root_depth));
    }
    let (root, visual) = (screen.root, screen.root_visual);
    let root_size = (
        i32::from(screen.width_in_pixels),
        i32::from(screen.height_in_pixels),
    );
    let randr = conn
        .extension_information(x11rb::protocol::randr::X11_EXTENSION_NAME)?
        .is_some();
    if conn
        .extension_information(shape::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Err(X11Error::NoShape);
    }
    let (atoms, randr) = intern_and_query_randr(&conn, randr)?;
    let max_bytes = conn.maximum_request_bytes();
    debug!(randr, max_bytes, "X11 display ready");
    Ok(Connected {
        conn,
        root,
        visual,
        root_size,
        max_bytes,
        atoms,
        randr,
    })
}

/// Interns the popup atoms and, when `RandR` is present, reports whether it
/// supports version 1.5 monitors, sending both requests before reading.
fn intern_and_query_randr(
    conn: &RustConnection<TimedStream>,
    randr: bool,
) -> Result<(Atoms, bool), X11Error> {
    let version = if randr {
        Some(conn.randr_query_version(1, 5)?)
    } else {
        None
    };
    let atoms = Atoms::intern(conn)?;
    let monitors = match version {
        Some(cookie) => {
            let version = cookie.reply()?;
            version.major_version > 1 || (version.major_version == 1 && version.minor_version >= 5)
        }
        None => false,
    };
    Ok((atoms, monitors))
}

/// The connection socket, registered with tokio for readiness only.
struct SocketFd(RawFd);

impl AsRawFd for SocketFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// One shown popup window.
struct PopupWindow {
    window: Window,
    gc: Gcontext,
    /// Pixels in `ZPixmap` order: B, G, R and an unused byte.
    data: Vec<u8>,
    size: (u16, u16),
}

/// X11 client state; owns every popup window.
pub(crate) struct X11Popups {
    /// Declared before `conn` so it deregisters before the socket closes.
    fd: AsyncFd<SocketFd>,
    conn: RustConnection<TimedStream>,
    root: Window,
    visual: Visualid,
    root_size: (i32, i32),
    max_bytes: usize,
    atoms: Atoms,
    randr: bool,
    position: Position,
    monitor: Monitor,
    scale: f32,
    font: FontState,
    icons: IconLoader,
    warn_limit: RateLimit,
    windows: BTreeMap<RequestId, PopupWindow>,
}

impl X11Popups {
    /// Registers the connection with the tokio reactor of the current runtime.
    pub(crate) fn new(connected: Connected, position: Position) -> Result<Self, X11Error> {
        let fd = AsyncFd::new(SocketFd(connected.conn.stream().as_raw_fd()))?;
        let (width, height) = connected.root_size;
        Ok(Self {
            fd,
            conn: connected.conn,
            root: connected.root,
            visual: connected.visual,
            root_size: connected.root_size,
            max_bytes: connected.max_bytes,
            atoms: connected.atoms,
            randr: connected.randr,
            position,
            monitor: Monitor {
                x: 0,
                y: 0,
                width,
                height,
                primary: true,
            },
            scale: 1.0,
            font: FontState::load(),
            icons: IconLoader::new(ICON_DEADLINE),
            warn_limit: RateLimit::new(WARN_INTERVAL),
            windows: BTreeMap::new(),
        })
    }

    /// Shows a popup for the prompt, or updates the one shown for its id.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn show(&mut self, prompt: &Prompt) -> Result<(), X11Error> {
        if self.windows.contains_key(&prompt.id) {
            return self.update(prompt).await;
        }
        self.scale = self.read_scale()?;
        self.monitor = self.focused_monitor()?;
        let Some((data, size, shape)) = self.render(prompt).await else {
            return Ok(());
        };
        let window = self.conn.generate_id()?;
        let gc = self.conn.generate_id()?;
        self.conn.create_window(
            DEPTH,
            window,
            self.root,
            0,
            0,
            size.0,
            size.1,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(0)
                .border_pixel(0)
                .event_mask(EventMask::EXPOSURE),
        )?;
        self.set_properties(window)?;
        self.set_shape(window, &shape)?;
        self.conn.create_gc(gc, window, &CreateGCAux::new())?;
        self.windows.insert(
            prompt.id,
            PopupWindow {
                window,
                gc,
                data,
                size,
            },
        );
        self.restack()?;
        self.conn.map_window(window)?;
        debug!(id = %prompt.id, width = size.0, height = size.1, scale = self.scale, "popup shown");
        Ok(())
    }

    /// Re-renders a shown popup in place.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn update(&mut self, prompt: &Prompt) -> Result<(), X11Error> {
        if !self.windows.contains_key(&prompt.id) {
            return Ok(());
        }
        let Some((data, size, shape)) = self.render(prompt).await else {
            return Ok(());
        };
        let Some(popup) = self.windows.get_mut(&prompt.id) else {
            return Ok(());
        };
        let window = popup.window;
        let resized = popup.size != size;
        popup.data = data;
        popup.size = size;
        if resized {
            self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .width(u32::from(size.0))
                    .height(u32::from(size.1)),
            )?;
            self.set_shape(window, &shape)?;
            self.restack()?;
        }
        self.draw(prompt.id)?;
        debug!(id = %prompt.id, width = size.0, height = size.1, "popup updated");
        Ok(())
    }

    /// Destroys the popup shown for `id`.
    #[instrument(level = "debug", skip_all, fields(%id))]
    pub(crate) fn hide(&mut self, id: RequestId) -> Result<(), X11Error> {
        if let Some(popup) = self.windows.remove(&id) {
            self.destroy(&popup)?;
            debug!(%id, "popup hidden");
            self.restack()?;
        }
        Ok(())
    }

    /// Destroys every popup.
    pub(crate) fn hide_all(&mut self) -> Result<(), X11Error> {
        debug!(count = self.windows.len(), "hiding all popups");
        let windows = std::mem::take(&mut self.windows);
        for popup in windows.values() {
            self.destroy(popup)?;
        }
        Ok(())
    }

    /// Handles buffered events and sends pending requests.
    pub(crate) fn dispatch(&mut self) -> Result<(), X11Error> {
        while let Some(event) = self.conn.poll_for_event()? {
            self.handle(event)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Waits until the socket is readable and returns the events read from
    /// it; cancel-safe.
    pub(crate) async fn read(&self) -> Result<Vec<Event>, X11Error> {
        let mut ready = self.fd.readable().await?;
        let mut events = Vec::new();
        while let Some(event) = self.conn.poll_for_event()? {
            events.push(event);
        }
        // `poll_for_event` reads the socket until it would block.
        ready.clear_ready();
        Ok(events)
    }

    /// Handles events returned by [`X11Popups::read`].
    pub(crate) fn handle_all(&mut self, events: Vec<Event>) -> Result<(), X11Error> {
        for event in events {
            self.handle(event)?;
        }
        Ok(())
    }

    fn handle(&mut self, event: Event) -> Result<(), X11Error> {
        match event {
            Event::Expose(expose) if expose.count == 0 => {
                let id = self
                    .windows
                    .iter()
                    .find(|(_, popup)| popup.window == expose.window)
                    .map(|(id, _)| *id);
                if let Some(id) = id {
                    self.draw(id)?;
                }
            }
            Event::Error(err) => {
                self.warn_limit.log(Instant::now(), |suppressed| {
                    warn!(
                        error_kind = ?err.error_kind,
                        request = err.major_opcode,
                        suppressed,
                        "X11 request failed"
                    );
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Returns the BGRX pixels, size and shape of the rendered popup.
    async fn render(&mut self, prompt: &Prompt) -> Option<(Vec<u8>, (u16, u16), Vec<Rectangle>)> {
        let icon = match prompt.icon.as_deref() {
            Some(path) => self.icons.get(path).await,
            None => None,
        };
        self.font.ready().await;
        let rendered = render(
            self.font.get(),
            &prompt.title,
            &display_body(prompt),
            icon.as_ref(),
            self.scale,
        );
        let Some(pixmap) = rendered else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "failed to render popup");
            });
            return None;
        };
        let (Ok(width), Ok(height)) = (
            u16::try_from(pixmap.width()),
            u16::try_from(pixmap.height()),
        ) else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "popup too large for X11");
            });
            return None;
        };
        let mut data = vec![0; pixmap.data().len()];
        copy_to_argb8888(pixmap.data(), &mut data);
        Some((data, (width, height), opaque_rows(&pixmap)))
    }

    /// Sends the pixels of the popup for `id`, split to fit the request size limit.
    fn draw(&self, id: RequestId) -> Result<(), X11Error> {
        let Some(popup) = self.windows.get(&id) else {
            return Ok(());
        };
        let stride = usize::from(popup.size.0) * 4;
        let Some(rows) = rows_per_request(self.max_bytes, stride) else {
            warn!(
                stride,
                max_bytes = self.max_bytes,
                "popup row exceeds the X11 request size"
            );
            return Ok(());
        };
        for (chunk, y) in popup
            .data
            .chunks(rows * stride)
            .zip((0_usize..).step_by(rows))
        {
            let (Ok(height), Ok(y)) = (u16::try_from(chunk.len() / stride), i16::try_from(y))
            else {
                break;
            };
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                popup.window,
                popup.gc,
                popup.size.0,
                height,
                0,
                y,
                0,
                DEPTH,
                chunk,
            )?;
        }
        Ok(())
    }

    fn destroy(&self, popup: &PopupWindow) -> Result<(), X11Error> {
        self.conn.destroy_window(popup.window)?;
        self.conn.free_gc(popup.gc)?;
        Ok(())
    }

    /// Stacks popups in id order away from the anchored edge of the monitor.
    fn restack(&self) -> Result<(), X11Error> {
        let margin = logical(MARGIN, self.scale);
        let spacing = logical(SPACING, self.scale);
        let mut offset = margin;
        for popup in self.windows.values() {
            let size = (i32::from(popup.size.0), i32::from(popup.size.1));
            let (x, y) = place(self.monitor, self.position, size, offset, margin);
            self.conn.configure_window(
                popup.window,
                &ConfigureWindowAux::new().x(i32::from(x)).y(i32::from(y)),
            )?;
            offset = offset.saturating_add(size.1).saturating_add(spacing);
        }
        Ok(())
    }

    /// Sets the EWMH and ICCCM properties of a popup window before it is mapped.
    fn set_properties(&self, window: Window) -> Result<(), X11Error> {
        let atoms = self.atoms;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.window_type,
            AtomEnum::ATOM,
            &[atoms.type_notification, atoms.type_utility],
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.state,
            AtomEnum::ATOM,
            &[
                atoms.state_above,
                atoms.state_skip_taskbar,
                atoms.state_skip_pager,
            ],
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.user_time,
            AtomEnum::CARDINAL,
            &[0],
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )?;
        self.conn
            .change_property8(PropMode::REPLACE, window, atoms.name, atoms.utf8, NAME)?;
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            WM_CLASS,
        )?;
        let mut hints = WmHints::new();
        hints.input = Some(false);
        hints.set(&self.conn, window)?;
        Ok(())
    }

    /// Cuts the window to its opaque pixels and makes it ignore input.
    fn set_shape(&self, window: Window, opaque: &[Rectangle]) -> Result<(), X11Error> {
        self.conn.shape_rectangles(
            SO::SET,
            SK::BOUNDING,
            ClipOrdering::Y_SORTED,
            window,
            0,
            0,
            opaque,
        )?;
        self.conn.shape_rectangles(
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        )?;
        Ok(())
    }

    /// Returns the scale from the `Xft.dpi` resource.
    fn read_scale(&self) -> Result<f32, X11Error> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                AtomEnum::RESOURCE_MANAGER,
                AtomEnum::STRING,
                0,
                MAX_RESOURCE_WORDS,
            )?
            .reply()?;
        if reply.format != 8 {
            return Ok(1.0);
        }
        let mut value = reply.value;
        if reply.bytes_after > 0 {
            // Drops the line cut by the length limit.
            let end = value
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |newline| newline + 1);
            value.truncate(end);
        }
        Ok(scale_from_dpi(dpi_from_property(&value)))
    }

    /// Returns the monitor under the pointer, or the whole root window.
    fn focused_monitor(&self) -> Result<Monitor, X11Error> {
        let pointer = self.conn.query_pointer(self.root)?;
        let monitors = if self.randr {
            Some(self.conn.randr_get_monitors(self.root, true)?)
        } else {
            None
        };
        let pointer = pointer.reply()?;
        let pointer = (i32::from(pointer.root_x), i32::from(pointer.root_y));
        let monitors: Vec<Monitor> = match monitors {
            Some(cookie) => cookie
                .reply()?
                .monitors
                .iter()
                .map(|monitor| Monitor {
                    x: i32::from(monitor.x),
                    y: i32::from(monitor.y),
                    width: i32::from(monitor.width),
                    height: i32::from(monitor.height),
                    primary: monitor.primary,
                })
                .collect(),
            None => Vec::new(),
        };
        let (width, height) = self.root_size;
        Ok(pick_monitor(&monitors, pointer).unwrap_or(Monitor {
            x: 0,
            y: 0,
            width,
            height,
            primary: true,
        }))
    }
}

/// Returns `length` logical pixels in device pixels.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is rounded and clamped to 0..=65535 first"
)]
fn logical(length: f32, scale: f32) -> i32 {
    (length * scale).round().clamp(0.0, 65_535.0) as i32
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn display_servers_follow_the_environment() {
        let set = OsString::from("x");
        let empty = OsString::new();
        assert_eq!(
            display_servers(Some(&set), None, Some(&set)),
            [DisplayServer::Wayland, DisplayServer::X11]
        );
        assert_eq!(
            display_servers(None, Some(&set), None),
            [DisplayServer::Wayland]
        );
        assert_eq!(
            display_servers(None, None, Some(&set)),
            [DisplayServer::X11]
        );
        assert_eq!(display_servers(Some(&empty), None, Some(&empty)), []);
        assert_eq!(display_servers(None, None, None), []);
    }

    fn scale(property: &str) -> f32 {
        scale_from_dpi(dpi_from_property(property.as_bytes()))
    }

    #[test]
    fn dpi_sets_the_scale() {
        assert!((scale("Xft.dpi: 96") - 1.0).abs() < f32::EPSILON);
        assert!((scale("Xft.dpi:\t192\n") - 2.0).abs() < f32::EPSILON);
        assert!((scale("Xft.dpi:  144.0 ") - 1.5).abs() < f32::EPSILON);
        assert!((scale("Xft.dpi: 72") - 1.0).abs() < f32::EPSILON);
        assert!((scale("Xft.dpi: 10000") - 4.0).abs() < f32::EPSILON);
        for invalid in [
            "",
            "Xft.dpi:",
            "Xft.dpi: abc",
            "Xft.dpi: -96",
            "Xft.dpi: NaN",
            "Xft.dpi: inf",
        ] {
            assert!((scale(invalid) - 1.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn dpi_is_read_from_resource_lines_only() {
        assert_eq!(
            dpi_from_property(b"#include \"/dev/zero\"\nXft.dpi: 144\n"),
            Some(144.0)
        );
        assert_eq!(
            dpi_from_property(b"Xcursor.size: 24\n  Xft.dpi :120\nXft.antialias: 1\n"),
            Some(120.0)
        );
        assert_eq!(
            dpi_from_property(b"Xft.dpi: 96\nXft.dpi: 192\n"),
            Some(192.0)
        );
        assert_eq!(dpi_from_property(b"!Xft.dpi: 144\nXft.dpix: 144\n"), None);
        assert_eq!(dpi_from_property(b"Xft.dpi: \xff\n"), None);
        assert_eq!(dpi_from_property(b"Xft.dpi 144\n"), None);
    }

    #[test]
    fn dpi_scan_stops_at_the_limit() {
        let filler = b"Xft.hinting: 1\n".repeat(70_000);
        assert!(filler.len() > 1024 * 1024);
        let mut early = b"Xft.dpi: 144\n".to_vec();
        early.extend_from_slice(&filler);
        assert_eq!(dpi_from_property(&early), Some(144.0));
        let mut late = filler;
        late.extend_from_slice(b"Xft.dpi: 144\n");
        assert_eq!(dpi_from_property(&late), None);
    }

    #[derive(Debug, thiserror::Error)]
    enum TimeoutError {
        #[error(transparent)]
        Io(#[from] io::Error),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
        #[error(transparent)]
        Connect(ConnectError),
        #[error("the handshake did not finish within 2 s")]
        Elapsed(#[from] tokio::time::error::Elapsed),
        #[error("the handshake succeeded without a server")]
        Connected,
    }

    #[tokio::test]
    async fn silent_server_times_out_the_handshake() -> Result<(), TimeoutError> {
        let (client, server) = std::os::unix::net::UnixStream::pair()?;
        let (stream, _) = DefaultStream::from_unix_stream(client)?;
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || handshake(stream, 0, Vec::new(), Vec::new())),
        )
        .await??;
        drop(server);
        match result {
            Err(ConnectError::IoError(err)) if err.kind() == io::ErrorKind::TimedOut => {
                assert!(started.elapsed() >= X11_IO_TIMEOUT);
                Ok(())
            }
            Err(err) => Err(TimeoutError::Connect(err)),
            Ok(_) => Err(TimeoutError::Connected),
        }
    }

    #[test]
    fn image_rows_fit_the_request_limit() {
        // 262140 bytes is the limit without BIG-REQUESTS.
        assert_eq!(rows_per_request(262_140, 360 * 4), Some(182));
        assert_eq!(rows_per_request(262_140, 262_116), Some(1));
        assert_eq!(rows_per_request(262_140, 262_117), None);
        assert_eq!(rows_per_request(10, 4), None);
        assert_eq!(rows_per_request(1000, 0), None);
    }

    fn monitor(x: i32, y: i32, width: i32, height: i32, primary: bool) -> Monitor {
        Monitor {
            x,
            y,
            width,
            height,
            primary,
        }
    }

    #[test]
    fn popups_are_placed_like_wayland() {
        let mon = monitor(0, 0, 1920, 1080, true);
        let size = (360, 100);
        assert_eq!(place(mon, Position::TopRight, size, 16, 16), (1544, 16));
        assert_eq!(place(mon, Position::TopLeft, size, 16, 16), (16, 16));
        assert_eq!(place(mon, Position::Top, size, 16, 16), (780, 16));
        assert_eq!(place(mon, Position::BottomLeft, size, 124, 16), (16, 856));
        assert_eq!(place(mon, Position::BottomRight, size, 16, 16), (1544, 964));
        assert_eq!(place(mon, Position::Bottom, size, 16, 16), (780, 964));
    }

    #[test]
    fn negative_origins_and_clamping() {
        let left = monitor(-1920, -200, 1920, 1080, false);
        assert_eq!(
            place(left, Position::TopLeft, (360, 100), 16, 16),
            (-1904, -184)
        );
        assert_eq!(
            place(left, Position::BottomRight, (360, 100), 16, 16),
            (-376, 764)
        );
        let far = monitor(40_000, -40_000, 1920, 1080, false);
        assert_eq!(
            place(far, Position::TopRight, (360, 100), 16, 16),
            (i16::MAX, i16::MIN)
        );
    }

    #[test]
    fn monitor_under_pointer_wins() {
        let monitors = [
            monitor(0, 0, 1920, 1080, false),
            monitor(1920, 0, 2560, 1440, true),
            monitor(-1280, 0, 1280, 1024, false),
        ];
        assert_eq!(pick_monitor(&monitors, (-5, 10)), Some(monitors[2]));
        assert_eq!(pick_monitor(&monitors, (100, 100)), Some(monitors[0]));
        assert_eq!(pick_monitor(&monitors, (1920, 1439)), Some(monitors[1]));
        assert_eq!(pick_monitor(&monitors, (9999, 9999)), Some(monitors[1]));
        assert_eq!(
            pick_monitor(&monitors[..1], (9999, 9999)),
            Some(monitors[0])
        );
        assert_eq!(pick_monitor(&[], (0, 0)), None);
    }

    fn shaped(opaque: &[usize]) -> Option<Vec<(i16, i16, u16, u16)>> {
        let mut pixmap = Pixmap::new(4, 4)?;
        let color = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 255)?;
        for index in opaque {
            *pixmap.pixels_mut().get_mut(*index)? = color;
        }
        let rects = opaque_rows(&pixmap);
        Some(
            rects
                .iter()
                .map(|r| (r.x, r.y, r.width, r.height))
                .collect(),
        )
    }

    #[test]
    fn shape_follows_alpha() {
        assert_eq!(
            shaped(&[1, 2, 5, 6, 9, 10, 12, 13, 14, 15]),
            Some(vec![(1, 0, 2, 3), (0, 3, 4, 1)])
        );
        assert_eq!(shaped(&[]), Some(vec![]));
        assert_eq!(shaped(&[0, 3]), Some(vec![(0, 0, 4, 1)]));
    }

    #[derive(Debug, thiserror::Error)]
    enum SmokeError {
        #[error(transparent)]
        X11(#[from] X11Error),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
    }

    /// Handles events and sends requests for `duration`.
    async fn pump(popups: &mut X11Popups, duration: std::time::Duration) -> Result<(), X11Error> {
        let end = tokio::time::Instant::now() + duration;
        loop {
            popups.dispatch()?;
            match tokio::time::timeout_at(end, popups.read()).await {
                Ok(events) => popups.handle_all(events?)?,
                Err(_deadline_reached) => return Ok(()),
            }
        }
    }

    fn smoke_prompt(id: u64, title: &str, state: touchcue_core::RequestState) -> Prompt {
        Prompt {
            id: RequestId(id),
            title: title.to_owned(),
            body: "touchcue x11_smoke is waiting for a FIDO2 touch; this text wraps onto a second line"
                .to_owned(),
            icon: None,
            state,
        }
    }

    /// Shows two stacked popups for about 3 s on the X11 display in `$DISPLAY`.
    #[tokio::test]
    #[ignore = "needs an X11 display in $DISPLAY"]
    async fn x11_smoke() -> Result<(), SmokeError> {
        use touchcue_core::{EndReason, RequestState};

        let connected = tokio::task::spawn_blocking(connect).await??;
        let mut popups = X11Popups::new(connected, Position::TopRight)?;
        popups
            .show(&smoke_prompt(
                1,
                "Touch your security key",
                RequestState::Waiting,
            ))
            .await?;
        popups
            .show(&smoke_prompt(2, "Second prompt", RequestState::Waiting))
            .await?;
        pump(&mut popups, std::time::Duration::from_millis(1500)).await?;
        let cancelled = RequestState::Lingering(EndReason::Cancelled);
        popups
            .update(&smoke_prompt(2, "Second prompt", cancelled))
            .await?;
        pump(&mut popups, std::time::Duration::from_millis(1500)).await?;
        popups.hide(RequestId(1))?;
        popups.hide(RequestId(2))?;
        popups.dispatch()?;
        Ok(())
    }
}
