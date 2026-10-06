//! Popups on whichever display server is available.

use std::time::Instant;

use touchcue_core::RequestId;
use tracing::{debug, instrument, warn};
use x11rb::protocol::Event;

use super::placement::Placement;
use super::wayland::{self, Popups, WaylandIo, WaylandIoError};
use super::x11::{self, DisplayServer, X11Error, X11Popups};
use crate::{INIT_TIMEOUT, Prompt};

/// Why a popup display connection stopped working.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OutputError {
    #[error(transparent)]
    Wayland(#[from] WaylandIoError),
    #[error(transparent)]
    X11(#[from] X11Error),
}

/// Events read from a display connection, handled by [`PopupOutput::handle`].
pub(crate) enum Incoming {
    /// Wayland events wait in the event queue for the next dispatch.
    Wayland,
    X11(Vec<Event>),
}

/// Popups on one display server together with its connection.
pub(crate) enum PopupOutput {
    Wayland { popups: Box<Popups>, io: WaylandIo },
    X11(Box<X11Popups>),
}

impl PopupOutput {
    /// Opens popups on the first display server named by the environment
    /// that supports them.
    #[instrument(skip_all, fields(position = ?placement.position))]
    pub(crate) async fn open(placement: &Placement) -> Option<Self> {
        let servers = x11::display_servers(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("WAYLAND_SOCKET").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        );
        for server in servers {
            let opened = match server {
                DisplayServer::Wayland => open_wayland(placement.clone()).await,
                DisplayServer::X11 => open_x11(placement.clone()).await,
            };
            if opened.is_some() {
                debug!(?server, "popups available");
                return opened;
            }
        }
        None
    }

    /// Shows the prompt; with `modal`, it holds overlays that block clicks.
    pub(crate) async fn show(&mut self, prompt: &Prompt, modal: bool) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.show(prompt, modal).await,
            Self::X11(popups) => popups.show(prompt, modal).await?,
        }
        Ok(())
    }

    /// Updates the prompt; without `modal`, it lets go of its overlays.
    pub(crate) async fn update(&mut self, prompt: &Prompt, modal: bool) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.update(prompt, modal).await,
            Self::X11(popups) => popups.update(prompt, modal).await?,
        }
        Ok(())
    }

    /// Removes the overlays that reached their time limit at `now`; the
    /// popups stay.
    pub(crate) fn expire_overlays(&mut self, now: Instant) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.expire_overlays(now),
            Self::X11(popups) => popups.expire_overlays(now)?,
        }
        Ok(())
    }

    /// Returns the earliest time [`PopupOutput::expire_overlays`] has work.
    pub(crate) fn next_overlay_deadline(&self) -> Option<Instant> {
        match self {
            Self::Wayland { popups, .. } => popups.next_overlay_deadline(),
            Self::X11(popups) => popups.next_overlay_deadline(),
        }
    }

    /// Returns the modal overlays and how many of them are drawn.
    #[cfg(test)]
    pub(crate) fn overlay_counts(&self) -> (usize, usize) {
        match self {
            Self::Wayland { popups, .. } => popups.overlay_counts(),
            Self::X11(popups) => popups.overlay_counts(),
        }
    }

    /// Returns how overlays are drawn.
    #[cfg(test)]
    pub(crate) fn overlay_paint(&self) -> &'static str {
        match self {
            Self::Wayland { popups, .. } => popups.overlay_paint(),
            Self::X11(_) => X11Popups::overlay_paint(),
        }
    }

    /// Returns the requests whose popups a click on an overlay hid since
    /// the last call.
    pub(crate) fn take_dismissed(&mut self) -> Vec<RequestId> {
        match self {
            Self::Wayland { popups, .. } => popups.take_dismissed(),
            Self::X11(popups) => popups.take_dismissed(),
        }
    }

    pub(crate) fn hide(&mut self, id: RequestId) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.hide(id),
            Self::X11(popups) => popups.hide(id)?,
        }
        Ok(())
    }

    pub(crate) fn hide_all(&mut self) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.hide_all(),
            Self::X11(popups) => popups.hide_all()?,
        }
        Ok(())
    }

    /// Handles queued events and sends pending requests.
    pub(crate) async fn dispatch(&mut self) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, io } => io.dispatch(popups).await?,
            Self::X11(popups) => popups.dispatch()?,
        }
        Ok(())
    }

    /// Waits until the connection has new events and reads them; cancel-safe.
    pub(crate) async fn read(&self) -> Result<Incoming, OutputError> {
        match self {
            Self::Wayland { io, .. } => {
                io.read().await?;
                Ok(Incoming::Wayland)
            }
            Self::X11(popups) => Ok(Incoming::X11(popups.read().await?)),
        }
    }

    /// Handles events returned by [`PopupOutput::read`].
    pub(crate) fn handle(&mut self, incoming: Incoming) -> Result<(), OutputError> {
        match (self, incoming) {
            (Self::X11(popups), Incoming::X11(events)) => popups.handle_all(events)?,
            (Self::Wayland { .. } | Self::X11(_), Incoming::Wayland | Incoming::X11(_)) => {}
        }
        Ok(())
    }
}

async fn open_wayland(placement: Placement) -> Option<PopupOutput> {
    let connected = match tokio::task::spawn_blocking(wayland::connect).await {
        Ok(connected) => connected,
        Err(err) => {
            warn!(
                error = &err as &dyn std::error::Error,
                "Wayland connect task failed"
            );
            return None;
        }
    };
    let opened = connected.and_then(|(conn, globals, queue)| {
        let popups = Popups::new(&globals, &queue, placement)?;
        let io = WaylandIo::new(conn, queue)?;
        Ok(PopupOutput::Wayland {
            popups: Box::new(popups),
            io,
        })
    });
    match opened {
        Ok(opened) => Some(opened),
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "Wayland popups unavailable"
            );
            None
        }
    }
}

async fn open_x11(placement: Placement) -> Option<PopupOutput> {
    let connected = tokio::time::timeout(INIT_TIMEOUT, tokio::task::spawn_blocking(x11::connect));
    let connected = match connected.await {
        Ok(Ok(connected)) => connected,
        Err(_) => {
            debug!(
                timeout_ms = INIT_TIMEOUT.as_millis(),
                "X11 connect timed out"
            );
            return None;
        }
        Ok(Err(err)) => {
            warn!(
                error = &err as &dyn std::error::Error,
                "X11 connect task failed"
            );
            return None;
        }
    };
    match connected.and_then(|connected| X11Popups::new(connected, placement)) {
        Ok(popups) => Some(PopupOutput::X11(Box::new(popups))),
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "X11 popups unavailable"
            );
            None
        }
    }
}
