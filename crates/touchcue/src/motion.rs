//! Whether the desktop asks for reduced motion.

use std::time::Duration;

use touchcue_ipc::portal::{Portal, PortalError};

/// Longest wait for connecting to the portal and reading both settings.
const PORTAL_DEADLINE: Duration = Duration::from_millis(1500);
/// `org.freedesktop.appearance` `reduced-motion`: 1 asks for reduced motion.
const APPEARANCE: (&str, &str) = ("org.freedesktop.appearance", "reduced-motion");
/// `org.gnome.desktop.interface` `enable-animations`: false turns animations off.
const GNOME: (&str, &str) = ("org.gnome.desktop.interface", "enable-animations");

/// Returns whether either available setting requests reduced motion.
pub fn requested(reduced_motion: Option<u32>, enable_animations: Option<bool>) -> bool {
    reduced_motion == Some(1) || enable_animations == Some(false)
}

/// Reads both desktop motion settings within [`PORTAL_DEADLINE`].
/// Failed or timed-out reads do not override an available preference.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn detect() -> bool {
    let deadline = tokio::time::Instant::now() + PORTAL_DEADLINE;
    let portal = match tokio::time::timeout_at(deadline, Portal::connect()).await {
        Ok(Ok(portal)) => portal,
        Ok(Err(err)) => {
            tracing::debug!(
                error = &err as &dyn std::error::Error,
                "no desktop portal; motion is not reduced"
            );
            return false;
        }
        Err(err) => {
            tracing::debug!(
                error = &err as &dyn std::error::Error,
                "desktop portal connection timed out; motion is not reduced"
            );
            return false;
        }
    };
    Box::pin(read(
        portal.read_u32(APPEARANCE.0, APPEARANCE.1),
        portal.read_bool(GNOME.0, GNOME.1),
        deadline,
    ))
    .await
}

/// Reads each setting independently until the shared deadline.
#[tracing::instrument(level = "debug", skip_all)]
async fn read(
    reduced_motion: impl Future<Output = Result<u32, PortalError>>,
    enable_animations: impl Future<Output = Result<bool, PortalError>>,
    deadline: tokio::time::Instant,
) -> bool {
    let (reduced_motion, enable_animations) = tokio::join!(
        setting(reduced_motion, APPEARANCE, deadline),
        setting(enable_animations, GNOME, deadline),
    );
    let reduced = requested(reduced_motion, enable_animations);
    tracing::debug!(
        ?reduced_motion,
        ?enable_animations,
        reduced,
        "desktop motion preference read"
    );
    reduced
}

/// Returns the setting, or `None` when its read fails or times out.
#[tracing::instrument(level = "debug", skip_all)]
async fn setting<T>(
    read: impl Future<Output = Result<T, PortalError>>,
    (namespace, key): (&str, &str),
    deadline: tokio::time::Instant,
) -> Option<T> {
    match tokio::time::timeout_at(deadline, read).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(err)) => {
            tracing::debug!(
                error = &err as &dyn std::error::Error,
                namespace,
                key,
                "portal setting unavailable"
            );
            None
        }
        Err(err) => {
            tracing::debug!(
                error = &err as &dyn std::error::Error,
                namespace,
                key,
                "portal setting timed out"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_setting_reduces_motion() {
        assert!(!requested(None, None));
        assert!(!requested(Some(0), Some(true)));
        assert!(!requested(Some(7), None));
        assert!(requested(Some(1), None));
        assert!(requested(None, Some(false)));
        assert!(requested(Some(1), Some(true)));
        assert!(requested(Some(0), Some(false)));
    }

    #[tokio::test]
    async fn available_preference_survives_other_read_timeout() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        assert!(read(async { Ok(1) }, std::future::pending(), deadline).await);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        assert!(read(std::future::pending(), async { Ok(false) }, deadline).await);
    }
}
