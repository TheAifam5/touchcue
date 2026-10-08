//! Click-through popups on the wlr-layer-shell overlay layer, with
//! optional modal overlays that block clicks behind them.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region, SurfaceData};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::backend::WaylandError;
use smithay_client_toolkit::reexports::client::globals::{GlobalList, registry_queue_init};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::reexports::client::protocol::{
    wl_buffer, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface, wl_touch,
};
use smithay_client_toolkit::reexports::client::{
    Connection, DispatchError, EventQueue, Proxy, QueueHandle,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
use smithay_client_toolkit::reexports::protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::seat::pointer::cursor_shape::CursorShapeManager;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
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
use touchcue_core::config::{OutputTarget, Position};
use touchcue_core::{RateLimit, RequestId};
use tracing::{debug, instrument, warn};

use super::WARN_INTERVAL;
use super::icon::{ICON_DEADLINE, IconLoader};
use super::modal::OverlayClock;
use super::placement::{Placement, hyprland_cursor_output, matching, stack};
use super::render::{FontState, WIDTH, copy_to_argb8888, render};
use crate::Prompt;
use crate::text::display_body;

const NAMESPACE: &str = "touchcue";
const MODAL_NAMESPACE: &str = "touchcue-modal";
/// Distance from the screen edges, in pixels.
const MARGIN: i32 = 16;
/// Vertical space between stacked popups, in pixels.
const SPACING: i32 = 8;
/// Initial size of the shared memory pool: one popup of 256 rows.
const POOL_BYTES: usize = WIDTH as usize * 256 * 4;
/// Linux input codes of the buttons that dismiss an overlay; others, such
/// as side buttons, do not.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
/// Largest shared-memory overlay buffer, in bytes: a 4K output at scale 2.
const MAX_OVERLAY_BYTES: u64 = 7680 * 4320 * 4;
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
    #[error("failed to read the Wayland outputs")]
    Outputs(#[source] DispatchError),
}

/// One shown popup on one output.
struct Surface {
    /// `None` lets the compositor pick the output.
    output: Option<wl_output::WlOutput>,
    layer: LayerSurface,
    pixmap: Pixmap,
    /// Size last requested with `set_size`; for centred popups, the size of
    /// the whole stack.
    size: (u32, u32),
    /// Row of the surface at which the pixmap is drawn.
    canvas_y: u32,
    configured: bool,
    margin: Option<i32>,
    /// Keeps the attached buffer until it is replaced.
    buffer: Option<Buffer>,
}

/// A full-output surface that dims the output and takes its pointer input
/// while the requests it holds wait.
struct Overlay {
    output: Option<wl_output::WlOutput>,
    layer: LayerSurface,
    /// Never empty: an overlay is destroyed when its last request lets go.
    holders: BTreeSet<RequestId>,
    /// Stretches `pixel` over the output.
    viewport: Option<WpViewport>,
    /// Single-pixel buffer of the dim colour.
    pixel: Option<wl_buffer::WlBuffer>,
    /// Shared-memory buffer, used without single-pixel buffers.
    buffer: Option<Buffer>,
    /// A buffer was attached and committed.
    drawn: bool,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(pixel) = self.pixel.take() {
            pixel.destroy();
        }
    }
}

/// Globals that draw an overlay as one pixel scaled to the output.
struct SinglePixel {
    viewporter: WpViewporter,
    manager: WpSinglePixelBufferManagerV1,
}

/// User data of the objects behind [`SinglePixel`], none of which has
/// events touchcue acts on.
struct OverlayData;

impl<I: Proxy, D> Dispatch2<I, D> for OverlayData {
    fn event(&self, _: &mut D, _: &I, _: I::Event, _: &Connection, _: &QueueHandle<D>) {}
}

/// The touch device of one seat.
struct SeatTouch {
    seat: wl_seat::WlSeat,
    touch: wl_touch::WlTouch,
}

/// The pointer of one seat, with its cursor-shape device when available.
struct SeatPointer {
    seat: wl_seat::WlSeat,
    pointer: wl_pointer::WlPointer,
    shape: Option<WpCursorShapeDeviceV1>,
}

/// Wayland client state; owns every popup surface.
pub(crate) struct Popups {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    /// Holds shared-memory overlay buffers; dropped when the last overlay is.
    modal_pool: Option<SlotPool>,
    /// `None` when the compositor lacks a global or popups are not modal.
    single_pixel: Option<SinglePixel>,
    qh: QueueHandle<Self>,
    placement: Placement,
    font: FontState,
    icons: IconLoader,
    /// Limits warnings about popups that cannot be drawn.
    warn_limit: RateLimit,
    /// Limits warnings about output targets that fall back to `focused`.
    target_limit: RateLimit,
    surfaces: BTreeMap<RequestId, Vec<Surface>>,
    overlays: Vec<Overlay>,
    overlay_clock: OverlayClock<Option<wl_output::WlOutput>>,
    /// Sets the cursor over overlays; `None` when the compositor lacks
    /// cursor-shape-v1 or popups are not modal.
    cursor_shape: Option<CursorShapeManager>,
    /// Pointers of seats, requested only for modal popups.
    pointers: Vec<SeatPointer>,
    /// Touch devices of seats, requested only when touches dismiss overlays.
    touches: Vec<SeatTouch>,
    /// Requests dismissed by a click since [`Popups::take_dismissed`].
    dismissed: Vec<RequestId>,
}

/// Connects to the compositor and lists its globals.
#[instrument(skip_all)]
pub(crate) fn connect() -> Result<(Connection, GlobalList, EventQueue<Popups>), PopupError> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    Ok((conn, globals, queue))
}

/// Connects and binds the popup globals, then waits one roundtrip so the
/// first popup can be placed on an output by name.
#[instrument(skip_all)]
pub(crate) fn open(
    placement: Placement,
) -> Result<(Connection, Popups, EventQueue<Popups>), PopupError> {
    let (conn, globals, mut queue) = connect()?;
    let mut popups = Popups::new(&globals, &queue, placement)?;
    queue.roundtrip(&mut popups).map_err(PopupError::Outputs)?;
    Ok((conn, popups, queue))
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
    #[instrument(skip_all, fields(position = ?placement.position))]
    pub(crate) fn new(
        globals: &GlobalList,
        queue: &EventQueue<Self>,
        placement: Placement,
    ) -> Result<Self, PopupError> {
        let qh = queue.handle();
        let layer_shell = LayerShell::bind(globals, &qh)?;
        let compositor = CompositorState::bind(globals, &qh)?;
        let shm = Shm::bind(globals, &qh)?;
        let pool = SlotPool::new(POOL_BYTES, &shm)?;
        let font = FontState::load();
        let cursor_shape = if placement.modal {
            match CursorShapeManager::bind(globals, &qh) {
                Ok(manager) => Some(manager),
                Err(err) => {
                    debug!(
                        error = &err as &dyn std::error::Error,
                        "cursor-shape-v1 unavailable; the cursor over modal overlays is unchanged"
                    );
                    None
                }
            }
        } else {
            None
        };
        let single_pixel = if placement.modal {
            match (
                globals.bind::<WpViewporter, _, _>(&qh, 1..=1, OverlayData),
                globals.bind::<WpSinglePixelBufferManagerV1, _, _>(&qh, 1..=1, OverlayData),
            ) {
                (Ok(viewporter), Ok(manager)) => Some(SinglePixel {
                    viewporter,
                    manager,
                }),
                (Err(err), _) | (_, Err(err)) => {
                    debug!(
                        error = &err as &dyn std::error::Error,
                        "single-pixel overlays unavailable; using shared-memory buffers"
                    );
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            registry_state: RegistryState::new(globals),
            output_state: OutputState::new(globals, &qh),
            seat_state: SeatState::new(globals, &qh),
            compositor,
            shm,
            layer_shell,
            pool,
            modal_pool: None,
            single_pixel,
            qh,
            placement,
            font,
            icons: IconLoader::new(ICON_DEADLINE),
            warn_limit: RateLimit::new(WARN_INTERVAL),
            target_limit: RateLimit::new(WARN_INTERVAL),
            surfaces: BTreeMap::new(),
            overlays: Vec::new(),
            overlay_clock: OverlayClock::default(),
            cursor_shape,
            pointers: Vec::new(),
            touches: Vec::new(),
            dismissed: Vec::new(),
        })
    }

    /// Shows a popup for the prompt on each target output, or updates the
    /// ones shown for its id; with `modal`, the request holds an overlay on
    /// each of those outputs.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn show(&mut self, prompt: &Prompt, modal: bool) {
        if self.surfaces.contains_key(&prompt.id) {
            self.update(prompt, modal).await;
            return;
        }
        let Some(pixmap) = self.render(prompt).await else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "failed to render popup");
            });
            return;
        };
        let targets = self.targets().await;
        if modal {
            // Overlays are created first, so the popups stack above them.
            for output in &targets {
                self.hold_overlay(prompt.id, output.as_ref());
            }
        }
        let mut surfaces = Vec::with_capacity(targets.len());
        for output in targets {
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(
                &self.qh,
                surface,
                Layer::Overlay,
                Some(NAMESPACE),
                output.as_ref(),
            );
            layer.set_anchor(anchor(self.placement.position));
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.set_exclusive_zone(0);
            match Region::new(&self.compositor) {
                Ok(region) => layer.set_input_region(Some(region.wl_region())),
                Err(err) => debug!(
                    error = &err as &dyn std::error::Error,
                    "popup input region unavailable"
                ),
            }
            surfaces.push(Surface {
                output,
                layer,
                pixmap: pixmap.clone(),
                size: (0, 0),
                canvas_y: 0,
                configured: false,
                margin: None,
                buffer: None,
            });
        }
        debug!(
            id = %prompt.id,
            width = pixmap.width(),
            height = pixmap.height(),
            outputs = surfaces.len(),
            modal,
            "popup shown"
        );
        self.surfaces.insert(prompt.id, surfaces);
        // Sends every new surface its size and initial commit without a
        // buffer, which it needs before its first configure.
        self.restack();
    }

    /// Re-renders the shown popups of a prompt in place; without `modal`,
    /// the request lets go of its overlays.
    #[instrument(level = "debug", skip_all, fields(id = %prompt.id, state = prompt.state.as_str()))]
    pub(crate) async fn update(&mut self, prompt: &Prompt, modal: bool) {
        if !modal {
            self.release(prompt.id);
        }
        if !self.surfaces.contains_key(&prompt.id) {
            return;
        }
        let Some(pixmap) = self.render(prompt).await else {
            self.warn_limit.log(Instant::now(), |suppressed| {
                warn!(suppressed, "failed to render popup");
            });
            return;
        };
        let Some(surfaces) = self.surfaces.get_mut(&prompt.id) else {
            return;
        };
        debug!(id = %prompt.id, width = pixmap.width(), height = pixmap.height(), "popup updated");
        for surface in surfaces.iter_mut() {
            surface.pixmap = pixmap.clone();
        }
        // A new size takes effect with the next configure, which redraws.
        self.restack();
        if let Some(surfaces) = self.surfaces.get_mut(&prompt.id) {
            for surface in surfaces.iter_mut().filter(|surface| surface.configured) {
                draw(&mut self.pool, &mut self.warn_limit, surface);
            }
        }
    }

    /// Destroys the popups and lets go of the overlays of `id`.
    #[instrument(level = "debug", skip_all, fields(%id))]
    pub(crate) fn hide(&mut self, id: RequestId) {
        self.release(id);
        if self.surfaces.remove(&id).is_some() {
            debug!(id = %id, "popup hidden");
            self.restack();
        }
    }

    /// Destroys every popup and overlay.
    pub(crate) fn hide_all(&mut self) {
        debug!(
            count = self.surfaces.len(),
            overlays = self.overlays.len(),
            "hiding all popups"
        );
        self.surfaces.clear();
        self.remove_overlays(|_| true);
        self.overlay_clock.clear();
    }

    /// Lets go of the overlays held by `id`, destroying those it held last.
    pub(crate) fn release(&mut self, id: RequestId) {
        for overlay in &mut self.overlays {
            overlay.holders.remove(&id);
        }
        let removed = self.remove_overlays(|overlay| overlay.holders.is_empty());
        if !removed.is_empty() {
            debug!(%id, remaining = self.overlays.len(), "modal overlays removed");
        }
        for output in &removed {
            self.overlay_clock.ended(output);
        }
    }

    /// Destroys the overlays that reached [`super::modal::MODAL_MAX`] at
    /// `now`; their outputs then cool down.
    pub(crate) fn expire_overlays(&mut self, now: Instant) {
        let expired = self.overlay_clock.expire(now);
        let removed = self.remove_overlays(|overlay| expired.contains(&overlay.output));
        if !removed.is_empty() {
            debug!(
                count = removed.len(),
                "modal overlays reached their time limit"
            );
        }
    }

    /// Returns the earliest time [`Popups::expire_overlays`] has work.
    pub(crate) fn next_overlay_deadline(&self) -> Option<Instant> {
        self.overlay_clock.next_deadline()
    }

    /// Returns the overlays and how many of them have a buffer committed.
    #[cfg(test)]
    pub(crate) fn overlay_counts(&self) -> (usize, usize) {
        let drawn = self.overlays.iter().filter(|overlay| overlay.drawn).count();
        (self.overlays.len(), drawn)
    }

    /// Returns how overlays are drawn.
    #[cfg(test)]
    pub(crate) fn overlay_paint(&self) -> &'static str {
        match self.single_pixel {
            Some(_) => "single-pixel buffer with viewport",
            None => "shared-memory buffer",
        }
    }

    /// Destroys the overlays matching `remove` and returns their outputs.
    fn remove_overlays(
        &mut self,
        remove: impl Fn(&Overlay) -> bool,
    ) -> Vec<Option<wl_output::WlOutput>> {
        let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.overlays)
            .into_iter()
            .partition(remove);
        self.overlays = kept;
        let outputs = removed
            .iter()
            .map(|overlay| overlay.output.clone())
            .collect();
        // Buffers go before the pool that holds them.
        drop(removed);
        if self.overlays.is_empty() {
            self.modal_pool = None;
        }
        outputs
    }

    /// Returns the requests whose popups a click dismissed since the last call.
    pub(crate) fn take_dismissed(&mut self) -> Vec<RequestId> {
        std::mem::take(&mut self.dismissed)
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

    /// Returns the outputs a new popup is shown on; `None` lets the
    /// compositor pick.
    async fn targets(&mut self) -> Vec<Option<wl_output::WlOutput>> {
        let wanted = match &self.placement.target {
            OutputTarget::Focused => return vec![None],
            OutputTarget::All => {
                let all: Vec<_> = self.output_state.outputs().map(Some).collect();
                if all.is_empty() {
                    self.fall_back("no outputs are known");
                    return vec![None];
                }
                return all;
            }
            OutputTarget::Cursor => match hyprland_cursor_output().await {
                Ok(Some(name)) => vec![name],
                Ok(None) => {
                    self.fall_back("the cursor is outside every output");
                    return vec![None];
                }
                Err(err) => {
                    self.target_limit.log(Instant::now(), |suppressed| {
                        warn!(
                            error = &err as &dyn std::error::Error,
                            suppressed, "output under the cursor unknown; using the focused output"
                        );
                    });
                    return vec![None];
                }
            },
            OutputTarget::Named(names) => names.clone(),
        };
        let outputs: Vec<_> = self.output_state.outputs().collect();
        let names: Vec<Option<String>> = outputs
            .iter()
            .map(|output| self.output_state.info(output).and_then(|info| info.name))
            .collect();
        let names: Vec<Option<&str>> = names.iter().map(Option::as_deref).collect();
        let found: Vec<_> = matching(&wanted, &names)
            .into_iter()
            .filter_map(|index| outputs.get(index).cloned().map(Some))
            .collect();
        if found.is_empty() {
            self.fall_back("no configured output exists");
            return vec![None];
        }
        found
    }

    fn fall_back(&mut self, reason: &'static str) {
        self.target_limit.log(Instant::now(), |suppressed| {
            warn!(reason, suppressed, "using the focused output");
        });
    }

    /// Adds `id` to the overlay on `output`, creating the overlay when
    /// the output has none.
    fn hold_overlay(&mut self, id: RequestId, output: Option<&wl_output::WlOutput>) {
        if let Some(overlay) = self
            .overlays
            .iter_mut()
            .find(|overlay| overlay.output.as_ref() == output)
        {
            overlay.holders.insert(id);
            return;
        }
        if !self.overlay_clock.start(&output.cloned(), Instant::now()) {
            debug!(%id, "output is cooling down; no modal overlay");
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(MODAL_NAMESPACE),
            output,
        );
        layer.set_anchor(Anchor::all());
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        // The keyboard stays with the focused window; the default input
        // region takes every click on the output.
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.overlays.push(Overlay {
            output: output.cloned(),
            layer,
            holders: BTreeSet::from([id]),
            viewport: None,
            pixel: None,
            buffer: None,
            drawn: false,
        });
        debug!(%id, "modal overlay shown");
    }

    /// Hides the popups of every request holding the overlay `index`.
    fn dismiss_overlay(&mut self, index: usize) {
        let Some(overlay) = self.overlays.get(index) else {
            return;
        };
        let holders: Vec<_> = overlay.holders.iter().copied().collect();
        for id in holders {
            self.release(id);
            self.surfaces.remove(&id);
            debug!(%id, "modal popup dismissed");
            self.dismissed.push(id);
        }
        self.restack();
    }

    /// Positions popups in id order, as one stack per output: away from the
    /// anchored edge, or centred.
    fn restack(&mut self) {
        let mut groups: Vec<Option<wl_output::WlOutput>> = Vec::new();
        for surface in self.surfaces.values().flatten() {
            let group = surface.stack_output();
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
        for group in groups {
            let mut members: Vec<&mut Surface> = self
                .surfaces
                .values_mut()
                .flatten()
                .filter(|surface| surface.stack_output() == group)
                .collect();
            let heights: Vec<i32> = members
                .iter()
                .map(|surface| clamp_i32(surface.pixmap.height()))
                .collect();
            let (offsets, total) = stack(&heights, SPACING);
            for (surface, offset) in members.iter_mut().zip(offsets) {
                place(
                    surface,
                    self.placement.position,
                    offset,
                    total,
                    &mut self.pool,
                    &mut self.warn_limit,
                );
            }
        }
    }
}

impl Surface {
    /// Returns the output whose stack the surface belongs to: the requested
    /// one, else the first output the compositor reports the surface on, so
    /// a popup left to the compositor's choice joins the popups there.
    fn stack_output(&self) -> Option<wl_output::WlOutput> {
        self.output.clone().or_else(|| {
            self.layer
                .wl_surface()
                .data::<SurfaceData<()>>()
                .and_then(|data| data.outputs().next())
        })
    }
}

/// Converts a pixel length to `i32`, saturating at `i32::MAX`.
fn clamp_i32(length: u32) -> i32 {
    length.min(i32::MAX.unsigned_abs()).cast_signed()
}

/// Moves a surface to `offset` pixels into a stack `total` pixels high,
/// committing only what changed.
fn place(
    surface: &mut Surface,
    position: Position,
    offset: i32,
    total: i32,
    pool: &mut SlotPool,
    limit: &mut RateLimit,
) {
    let (size, canvas_y, margin) = match position {
        // Each surface spans the whole stack, so the compositor centres all
        // of them in the same place and each popup is drawn at its offset.
        Position::Center => (
            (surface.pixmap.width(), total.max(0).unsigned_abs()),
            offset.max(0).unsigned_abs(),
            None,
        ),
        _ => (
            (surface.pixmap.width(), surface.pixmap.height()),
            0,
            Some(MARGIN.saturating_add(offset)),
        ),
    };
    let mut commit = false;
    if surface.margin != margin
        && let Some(margin) = margin
    {
        match edge(position) {
            Edge::Top => surface.layer.set_margin(margin, MARGIN, 0, MARGIN),
            Edge::Bottom => surface.layer.set_margin(0, MARGIN, margin, MARGIN),
        }
        surface.margin = Some(margin);
        commit = true;
    }
    if surface.size != size {
        surface.size = size;
        surface.canvas_y = canvas_y;
        surface.configured = false;
        surface.layer.set_size(size.0, size.1);
        commit = true;
    } else if surface.canvas_y != canvas_y {
        surface.canvas_y = canvas_y;
        if surface.configured {
            draw(pool, limit, surface);
        }
    }
    if commit {
        surface.layer.commit();
    }
}

enum Edge {
    Top,
    Bottom,
}

/// Returns the edge popups stack away from; centred popups use [`Edge::Top`].
fn edge(position: Position) -> Edge {
    match position {
        Position::TopLeft | Position::TopRight | Position::Top | Position::Center => Edge::Top,
        Position::BottomLeft | Position::BottomRight | Position::Bottom => Edge::Bottom,
    }
}

fn anchor(position: Position) -> Anchor {
    match position {
        Position::Center => Anchor::empty(),
        Position::TopLeft => Anchor::TOP | Anchor::LEFT,
        Position::TopRight => Anchor::TOP | Anchor::RIGHT,
        Position::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        Position::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
        Position::Top => Anchor::TOP,
        Position::Bottom => Anchor::BOTTOM,
    }
}

/// Attaches the surface's pixmap at its canvas row in a new buffer of the
/// surface's size and commits it.
fn draw(pool: &mut SlotPool, limit: &mut RateLimit, surface: &mut Surface) {
    let (Ok(width), Ok(height)) = (i32::try_from(surface.size.0), i32::try_from(surface.size.1))
    else {
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
    canvas.fill(0);
    let data = surface.pixmap.data();
    let start = u64::from(surface.canvas_y).saturating_mul(u64::from(stride.unsigned_abs()));
    let target = match usize::try_from(start) {
        Ok(start) => start
            .checked_add(data.len())
            .and_then(|end| canvas.get_mut(start..end)),
        Err(_) => None,
    };
    let Some(target) = target else {
        limit.log(Instant::now(), |suppressed| {
            warn!(suppressed, "popup does not fit its surface; not drawing");
        });
        return;
    };
    copy_to_argb8888(data, target);
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

/// Attaches one pixel of black at `alpha` to the overlay, scaled to
/// `width` by `height`, and commits it.
fn draw_single_pixel(
    single_pixel: &SinglePixel,
    qh: &QueueHandle<Popups>,
    overlay: &mut Overlay,
    (width, height): (i32, i32),
    alpha: u8,
) {
    let wl_surface = overlay.layer.wl_surface().clone();
    let viewport = overlay.viewport.get_or_insert_with(|| {
        single_pixel
            .viewporter
            .get_viewport(&wl_surface, qh, OverlayData)
    });
    viewport.set_destination(width, height);
    let pixel = overlay.pixel.get_or_insert_with(|| {
        // Premultiplied black: colour channels are 0 and alpha spans u32.
        single_pixel.manager.create_u32_rgba_buffer(
            0,
            0,
            0,
            u32::from(alpha).saturating_mul(0x0101_0101),
            qh,
            OverlayData,
        )
    });
    wl_surface.attach(Some(pixel), 0, 0);
    wl_surface.damage_buffer(0, 0, 1, 1);
    overlay.layer.commit();
    overlay.drawn = true;
}

/// Fills a new `width` by `height` buffer with black of `alpha`, creating
/// the overlay pool on first use, and commits it on the overlay.
fn draw_overlay(
    pool: &mut Option<SlotPool>,
    shm: &Shm,
    limit: &mut RateLimit,
    overlay: &mut Overlay,
    (width, height): (i32, i32),
    alpha: u8,
) {
    let stride = width.saturating_mul(4);
    let bytes = u64::from(stride.unsigned_abs()).saturating_mul(u64::from(height.unsigned_abs()));
    let bytes = match usize::try_from(bytes) {
        Ok(bytes) if bytes > 0 && bytes as u64 <= MAX_OVERLAY_BYTES => bytes,
        Ok(_) | Err(_) => {
            limit.log(Instant::now(), |suppressed| {
                warn!(
                    width,
                    height,
                    max_bytes = MAX_OVERLAY_BYTES,
                    suppressed,
                    "overlay too large; not drawing"
                );
            });
            return;
        }
    };
    if pool.is_none() {
        match SlotPool::new(bytes, shm) {
            Ok(created) => *pool = Some(created),
            Err(err) => {
                limit.log(Instant::now(), |suppressed| {
                    warn!(
                        error = &err as &dyn std::error::Error,
                        suppressed, "failed to create the overlay pool"
                    );
                });
                return;
            }
        }
    }
    let Some(pool) = pool else {
        return;
    };
    let (buffer, canvas) = match pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888)
    {
        Ok(created) => created,
        Err(err) => {
            limit.log(Instant::now(), |suppressed| {
                warn!(
                    error = &err as &dyn std::error::Error,
                    suppressed, "failed to allocate an overlay buffer"
                );
            });
            return;
        }
    };
    // Premultiplied black: only the alpha byte is set.
    for pixel in canvas.as_chunks_mut::<4>().0 {
        *pixel = [0, 0, 0, alpha];
    }
    let wl_surface = overlay.layer.wl_surface();
    wl_surface.damage_buffer(0, 0, width, height);
    if let Err(err) = buffer.attach_to(wl_surface) {
        limit.log(Instant::now(), |suppressed| {
            warn!(
                error = &err as &dyn std::error::Error,
                suppressed, "failed to attach an overlay buffer"
            );
        });
        return;
    }
    overlay.layer.commit();
    overlay.buffer = Some(buffer);
    overlay.drawn = true;
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
        surface: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        self.outputs_changed(surface);
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        self.outputs_changed(surface);
    }
}

impl Popups {
    /// Restacks after the compositor moved the surface of a popup without a
    /// requested output, which may move it to another output's stack.
    fn outputs_changed(&mut self, surface: &wl_surface::WlSurface) {
        let moved = self.surfaces.iter().find_map(|(id, popups)| {
            popups
                .iter()
                .any(|popup| popup.output.is_none() && popup.layer.wl_surface() == surface)
                .then_some(*id)
        });
        if let Some(id) = moved {
            debug!(%id, "popup output changed; restacking");
            self.restack();
        }
    }
}

impl OutputHandler for Popups {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let on_output = |other: &Option<wl_output::WlOutput>| other.as_ref() == Some(&output);
        let overlays = self
            .remove_overlays(|overlay| on_output(&overlay.output))
            .len();
        self.overlay_clock.ended(&Some(output.clone()));
        let mut removed = 0_usize;
        for surfaces in self.surfaces.values_mut() {
            let before = surfaces.len();
            surfaces.retain(|surface| !on_output(&surface.output));
            removed = removed.saturating_add(before - surfaces.len());
        }
        self.surfaces.retain(|_, surfaces| !surfaces.is_empty());
        if removed > 0 || overlays > 0 {
            debug!(
                popups = removed,
                overlays, "output removed; dropped its surfaces"
            );
            self.restack();
        }
    }
}

impl LayerShellHandler for Popups {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        let removed = self.remove_overlays(|overlay| &overlay.layer == layer);
        if !removed.is_empty() {
            debug!("compositor closed a modal overlay");
            for output in &removed {
                self.overlay_clock.ended(output);
            }
            return;
        }
        let mut closed = false;
        for surfaces in self.surfaces.values_mut() {
            let before = surfaces.len();
            surfaces.retain(|surface| &surface.layer != layer);
            closed |= surfaces.len() != before;
        }
        if closed {
            self.surfaces.retain(|_, surfaces| !surfaces.is_empty());
            debug!("compositor closed a popup");
            self.restack();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(overlay) = self
            .overlays
            .iter_mut()
            .find(|overlay| &overlay.layer == layer)
        {
            let (width, height) = configure.new_size;
            let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
                self.warn_limit.log(Instant::now(), |suppressed| {
                    warn!(
                        width,
                        height, suppressed, "overlay size out of range; not drawing"
                    );
                });
                return;
            };
            if width == 0 || height == 0 {
                return;
            }
            let alpha = self.placement.dim_alpha;
            match &self.single_pixel {
                Some(single_pixel) => {
                    draw_single_pixel(single_pixel, qh, overlay, (width, height), alpha);
                }
                None => draw_overlay(
                    &mut self.modal_pool,
                    &self.shm,
                    &mut self.warn_limit,
                    overlay,
                    (width, height),
                    alpha,
                ),
            }
            return;
        }
        let (pool, limit) = (&mut self.pool, &mut self.warn_limit);
        if let Some(surface) = self
            .surfaces
            .values_mut()
            .flatten()
            .find(|surface| &surface.layer == layer)
        {
            surface.configured = true;
            draw(pool, limit, surface);
        }
    }
}

impl SeatHandler for Popups {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Touch
            && self.placement.dismiss
            && !self.touches.iter().any(|known| known.seat == seat)
        {
            match self.seat_state.get_touch(qh, &seat) {
                Ok(touch) => self.touches.push(SeatTouch { seat, touch }),
                Err(err) => debug!(
                    error = &err as &dyn std::error::Error,
                    "touch unavailable; touches cannot dismiss modal popups"
                ),
            }
            return;
        }
        if capability != Capability::Pointer
            || !self.placement.modal
            || self.pointers.iter().any(|known| known.seat == seat)
        {
            return;
        }
        match self.seat_state.get_pointer(qh, &seat) {
            Ok(pointer) => {
                let shape = self
                    .cursor_shape
                    .as_ref()
                    .map(|manager| manager.get_shape_device(&pointer, qh));
                self.pointers.push(SeatPointer {
                    seat,
                    pointer,
                    shape,
                });
            }
            Err(err) => debug!(
                error = &err as &dyn std::error::Error,
                "pointer unavailable; modal overlays cannot set the cursor or be dismissed"
            ),
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer => self.release_pointer(&seat),
            Capability::Touch => self.release_touch(&seat),
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.release_pointer(&seat);
        self.release_touch(&seat);
    }
}

impl Popups {
    fn release_pointer(&mut self, seat: &wl_seat::WlSeat) {
        self.pointers.retain(|known| {
            let keep = &known.seat != seat;
            if !keep {
                if let Some(shape) = &known.shape {
                    shape.destroy();
                }
                known.pointer.release();
            }
            keep
        });
    }
}

impl Popups {
    fn release_touch(&mut self, seat: &wl_seat::WlSeat) {
        self.touches.retain(|known| {
            let keep = &known.seat != seat;
            if !keep {
                known.touch.release();
            }
            keep
        });
    }
}

impl TouchHandler for Popups {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        _: u32,
        surface: wl_surface::WlSurface,
        _: i32,
        _: (f64, f64),
    ) {
        if !self.placement.dismiss {
            return;
        }
        if let Some(index) = self
            .overlays
            .iter()
            .position(|overlay| overlay.layer.wl_surface() == &surface)
        {
            self.dismiss_overlay(index);
        }
    }

    fn up(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        _: u32,
        _: i32,
    ) {
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        _: i32,
        _: (f64, f64),
    ) {
    }

    fn shape(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
        _: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
    ) {
    }

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch) {}
}

impl PointerHandler for Popups {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(index) = self
                .overlays
                .iter()
                .position(|overlay| overlay.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    let shape = self
                        .pointers
                        .iter()
                        .find(|known| &known.pointer == pointer)
                        .and_then(|known| known.shape.as_ref());
                    if let Some(shape) = shape {
                        shape.set_shape(serial, Shape::Default);
                    }
                }
                PointerEventKind::Press { button, .. }
                    if self.placement.dismiss
                        && matches!(button, BTN_LEFT | BTN_RIGHT | BTN_MIDDLE) =>
                {
                    self.dismiss_overlay(index);
                }
                _ => {}
            }
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
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Popups);
delegate_dispatch2!(Popups);
