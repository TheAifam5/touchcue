//! Popups on whichever display server is available.

use touchcue_core::RequestId;
use touchcue_core::config::Position;
use tracing::{debug, instrument, warn};
use x11rb::protocol::Event;

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
    Wayland { popups: Popups, io: WaylandIo },
    X11(X11Popups),
}

impl PopupOutput {
    /// Opens popups on the first display server named by the environment
    /// that supports them.
    #[instrument(skip_all, fields(?position))]
    pub(crate) async fn open(position: Position) -> Option<Self> {
        let servers = x11::display_servers(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("WAYLAND_SOCKET").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        );
        for server in servers {
            let opened = match server {
                DisplayServer::Wayland => open_wayland(position).await,
                DisplayServer::X11 => open_x11(position).await,
            };
            if opened.is_some() {
                debug!(?server, "popups available");
                return opened;
            }
        }
        None
    }

    pub(crate) async fn show(&mut self, prompt: &Prompt) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.show(prompt).await,
            Self::X11(popups) => popups.show(prompt).await?,
        }
        Ok(())
    }

    pub(crate) async fn update(&mut self, prompt: &Prompt) -> Result<(), OutputError> {
        match self {
            Self::Wayland { popups, .. } => popups.update(prompt).await,
            Self::X11(popups) => popups.update(prompt).await?,
        }
        Ok(())
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

async fn open_wayland(position: Position) -> Option<PopupOutput> {
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
        let popups = Popups::new(&globals, &queue, position)?;
        let io = WaylandIo::new(conn, queue)?;
        Ok(PopupOutput::Wayland { popups, io })
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

async fn open_x11(position: Position) -> Option<PopupOutput> {
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
    match connected.and_then(|connected| X11Popups::new(connected, position)) {
        Ok(popups) => Some(PopupOutput::X11(popups)),
        Err(err) => {
            debug!(
                error = &err as &dyn std::error::Error,
                "X11 popups unavailable"
            );
            None
        }
    }
}
