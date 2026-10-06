//! Click-through popups on the wlr-layer-shell overlay layer.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::backend::WaylandError;
use smithay_client_toolkit::reexports::client::globals::{GlobalList, registry_queue_init};
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{
    Connection, DispatchError, EventQueue, QueueHandle,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use tiny_skia::Pixmap;
use tokio::io::unix::AsyncFd;
use touchcue_core::config::Position;
use touchcue_core::{RateLimit, RequestId};
use tracing::{debug, instrument, warn};

use super::WARN_INTERVAL;
use super::icon::{ICON_DEADLINE, IconLoader};
use super::render::{FontState, WIDTH, copy_to_argb8888, render};
use crate::Prompt;
use crate::text::display_body;

const NAMESPACE: &str = "touchcue";
/// Distance from the screen edges, in pixels.
const MARGIN: i32 = 16;
/// Vertical space between stacked popups, in pixels.
const SPACING: i32 = 8;
/// Initial size of the shared memory pool: one popup of 256 rows.
const POOL_BYTES: usize = WIDTH as usize * 256 * 4;
/// Longest time a flush waits for the compositor to accept requests.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Why popups are unavailable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PopupError {
    #[error("no Wayland compositor")]
    Connect(#[from] smithay_client_toolkit::reexports::client::ConnectError),
    #[error("failed to list Wayland globals")]
    Globals(#[from] smithay_client_toolkit::reexports::client::globals::GlobalError),
    #[error("required Wayland global is missing")]
    Bind(#[from] smithay_client_toolkit::reexports::client::globals::BindError),
    #[error("failed to create the shared memory pool")]
    Pool(#[from] smithay_client_toolkit::shm::CreatePoolError),
    #[error("failed to register the Wayland socket")]
    Register(#[from] io::Error),
}

/// One shown popup.
struct Surface {
    layer: LayerSurface,
    pixmap: Pixmap,
    /// Size last requested with `set_size`.
    size: (u32, u32),
    configured: bool,
    margin: Option<i32>,
    /// Keeps the attached buffer until it is replaced.
    buffer: Option<Buffer>,
}

/// Wayland client state; owns every popup surface.
pub(crate) struct Popups {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    qh: QueueHandle<Self>,
    position: Position,
    font: FontState,
    icons: IconLoader,
    /// Limits warnings about popups that cannot be drawn.
    warn_limit: RateLimit,
    surfaces: BTreeMap<RequestId, Surface>,
}

/// Connects to the compositor and lists its globals.
#[instrument(skip_all)]
pub(crate) fn connect() -> Result<(Connection, GlobalList, EventQueue<Popups>), PopupError> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    Ok((conn, globals, queue))
}

/// Why the Wayland connection stopped working.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WaylandIoError {
    #[error("failed to dispatch Wayland events")]
    Dispatch(#[from] DispatchError),
    #[error("Wayland connection failed")]
    Wayland(#[from] WaylandError),
    #[error("Wayland socket polling failed")]
    Io(#[from] io::Error),
    #[error("the compositor did not accept requests within {FLUSH_TIMEOUT:?}")]
    FlushTimeout(#[source] tokio::time::error::Elapsed),
}

/// The connection socket, registered with tokio for readiness only: reads
/// and writes go through `wayland-client`.
struct WaylandFd(Connection);

impl AsRawFd for WaylandFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.backend().poll_fd().as_fd().as_raw_fd()
    }
}

/// Event queue of the popups and readiness of its connection.
pub(crate) struct WaylandIo {
    queue: EventQueue<Popups>,
    fd: AsyncFd<WaylandFd>,
}

impl WaylandIo {
    /// Registers the connection with the tokio reactor of the current runtime.
    pub(crate) fn new(conn: Connection, queue: EventQueue<Popups>) -> io::Result<Self> {
        Ok(Self {
            queue,
            fd: AsyncFd::new(WaylandFd(conn))?,
        })
    }

    /// Dispatches queued events to `popups`, then sends pending requests,
    /// waiting up to [`FLUSH_TIMEOUT`] while the socket is full.
    pub(crate) async fn dispatch(&mut self, popups: &mut Popups) -> Result<(), WaylandIoError> {
        self.queue.dispatch_pending(popups)?;
        loop {
            match self.queue.flush() {
                Ok(()) => return Ok(()),
                Err(WaylandError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                    let mut ready = tokio::time::timeout(FLUSH_TIMEOUT, self.fd.writable())
                        .await
                        .map_err(WaylandIoError::FlushTimeout)??;
                    ready.clear_ready();
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Waits until the socket is readable and reads its events into the
    /// queue; cancel-safe.
    pub(crate) async fn read(&self) -> Result<(), WaylandIoError> {
        let mut ready = self.fd.readable().await?;
        // `None` means events are already queued; the next dispatch handles them.
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        match guard.read() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                ready.clear_ready();
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// Reports whether the compositor offers layer-shell, binding it once.
pub(crate) fn has_layer_shell(globals: &GlobalList, queue: &EventQueue<Popups>) -> bool {
    match LayerShell::bind(globals, &queue.handle()) {
        Ok(_) => true,
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "layer-shell unavailable"
            );
            false
        }
    }
}

impl Popups {
    /// Binds the globals popups need.
    ///
    /// Returns [`PopupError::Bind`] when the compositor has no layer-shell.
    #[instrument(skip_all, fields(?position))]
    pub(crate) fn new(
        globals: &GlobalList,
        queue: &EventQueue<Self>,
        position: Position,
    ) -> Result<Self, PopupError> {
        let qh = queue.handle();
        let layer_shell = LayerShell::bind(globals, &qh)?;
        let compositor = CompositorState::bind(globals, &qh)?;
        let shm = Shm::bind(globals, &qh)?;
        let pool = SlotPool::new(POOL_BYTES, &shm)?;
        let font = FontState::load();
        Ok(Self {
            registry_state: RegistryState::new(globals),
            output_state: OutputState::new(globals, &qh),
            compositor,
            shm,
            layer_shell,
            pool,
            qh,
            position,
            font,
            icons: IconLoader::new(ICON_DEADLINE),
            warn_limit: RateLimit::new(WARN_INTERVAL),
            surfaces: BTreeMap::new(),
        })
    }

    /// Shows a popup for the prompt, or updates the one shown for its id.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn show(&mut self, prompt: &Prompt) {
        if self.surfaces.contains_key(&prompt.id) {
            self.update(prompt).await;
            return;
        }
        let Some(pixmap) = self.render(prompt).await else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "failed to render popup");
            });
            return;
        };
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            None,
        );
        layer.set_anchor(anchor(self.position));
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(0);
        let size = (pixmap.width(), pixmap.height());
        layer.set_size(size.0, size.1);
        match Region::new(&self.compositor) {
            Ok(region) => layer.set_input_region(Some(region.wl_region())),
            Err(err) => debug!(
                error = &err as &dyn std::error::Error,
                "popup input region unavailable"
            ),
        }
        self.surfaces.insert(
            prompt.id,
            Surface {
                layer,
                pixmap,
                size,
                configured: false,
                margin: None,
                buffer: None,
            },
        );
        // Commits every surface whose margin changed, including the initial
        // commit without a buffer that this surface needs before its first configure.
        debug!(id = %prompt.id, width = size.0, height = size.1, "popup shown");
        self.restack();
    }

    /// Re-renders a shown popup in place.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn update(&mut self, prompt: &Prompt) {
        let Some(pixmap) = self.render(prompt).await else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "failed to render popup");
            });
            return;
        };
        let Some(surface) = self.surfaces.get_mut(&prompt.id) else {
            return;
        };
        let size = (pixmap.width(), pixmap.height());
        surface.pixmap = pixmap;
        debug!(id = %prompt.id, width = size.0, height = size.1, "popup updated");
        if size == surface.size {
            if surface.configured {
                draw(&mut self.pool, &mut self.warn_limit, surface);
            }
            return;
        }
        // A new size takes effect with the next configure, which redraws.
        surface.size = size;
        surface.configured = false;
        surface.layer.set_size(size.0, size.1);
        surface.layer.commit();
        self.restack();
    }

    /// Destroys the popup shown for `id`.
    #[instrument(level = "debug", skip_all, fields(%id))]
    pub(crate) fn hide(&mut self, id: RequestId) {
        if self.surfaces.remove(&id).is_some() {
            debug!(id = %id, "popup hidden");
            self.restack();
        }
    }

    /// Destroys every popup.
    pub(crate) fn hide_all(&mut self) {
        debug!(count = self.surfaces.len(), "hiding all popups");
        self.surfaces.clear();
    }

    async fn render(&mut self, prompt: &Prompt) -> Option<Pixmap> {
        let icon = match prompt.icon.as_deref() {
            Some(path) => self.icons.get(path).await,
            None => None,
        };
        self.font.ready().await;
        render(
            self.font.get(),
            &prompt.title,
            &display_body(prompt),
            icon.as_ref(),
            1.0,
        )
    }

    /// Stacks popups in id order away from the anchored edge.
    fn restack(&mut self) {
        let vertical = match self.position {
            Position::TopLeft | Position::TopRight | Position::Top => Edge::Top,
            Position::BottomLeft | Position::BottomRight | Position::Bottom => Edge::Bottom,
        };
        let mut offset = MARGIN;
        for surface in self.surfaces.values_mut() {
            if surface.margin != Some(offset) {
                surface.margin = Some(offset);
                match vertical {
                    Edge::Top => surface.layer.set_margin(offset, MARGIN, 0, MARGIN),
                    Edge::Bottom => surface.layer.set_margin(0, MARGIN, offset, MARGIN),
                }
                surface.layer.commit();
            }
            let height = match i32::try_from(surface.size.1) {
                Ok(height) => height,
                Err(err) => {
                    warn!(
                        error = &err as &dyn std::error::Error,
                        "popup height out of range"
                    );
                    i32::MAX
                }
            };
            offset = offset.saturating_add(height).saturating_add(SPACING);
        }
    }
}

enum Edge {
    Top,
    Bottom,
}

fn anchor(position: Position) -> Anchor {
    match position {
        Position::TopLeft => Anchor::TOP | Anchor::LEFT,
        Position::TopRight => Anchor::TOP | Anchor::RIGHT,
        Position::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        Position::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
        Position::Top => Anchor::TOP,
        Position::Bottom => Anchor::BOTTOM,
    }
}

/// Attaches the surface's pixmap in a new buffer and commits it.
fn draw(pool: &mut SlotPool, limit: &mut RateLimit, surface: &mut Surface) {
    let (Ok(width), Ok(height)) = (
        i32::try_from(surface.pixmap.width()),
        i32::try_from(surface.pixmap.height()),
    ) else {
        limit.log(Instant::now(), |suppressed| {
            warn!(suppressed, "popup size out of range; not drawing");
        });
        return;
    };
    let stride = width.saturating_mul(4);
    let (buffer, canvas) = match pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888)
    {
        Ok(created) => created,
        Err(err) => {
            limit.log(Instant::now(), |suppressed| {
                warn!(
                    error = &err as &dyn std::error::Error,
                    suppressed, "failed to allocate a popup buffer"
                );
            });
            return;
        }
    };
    copy_to_argb8888(surface.pixmap.data(), canvas);
    let wl_surface = surface.layer.wl_surface();
    wl_surface.damage_buffer(0, 0, width, height);
    if let Err(err) = buffer.attach_to(wl_surface) {
        limit.log(Instant::now(), |suppressed| {
            warn!(
                error = &err as &dyn std::error::Error,
                suppressed, "failed to attach a popup buffer"
            );
        });
        return;
    }
    surface.layer.commit();
    surface.buffer = Some(buffer);
}

impl CompositorHandler for Popups {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Popups {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Popups {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        let before = self.surfaces.len();
        self.surfaces.retain(|_, surface| &surface.layer != layer);
        if self.surfaces.len() != before {
            debug!("compositor closed a popup");
            self.restack();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (pool, limit) = (&mut self.pool, &mut self.warn_limit);
        if let Some(surface) = self
            .surfaces
            .values_mut()
            .find(|surface| &surface.layer == layer)
        {
            surface.configured = true;
            draw(pool, limit, surface);
        }
    }
}

impl ShmHandler for Popups {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Popups {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_registry!(Popups);
delegate_dispatch2!(Popups);
