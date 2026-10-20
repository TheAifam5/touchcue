//! Desktop settings read through the XDG desktop portal.

use std::time::Duration;

use tokio::time::timeout;
use zbus::fdo::DBusProxy;
use zbus::names::WellKnownName;
use zbus::{Connection, OwnedVariant, Variant};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Settings";
/// Upper bound on connecting to the session bus and on each portal call.
pub const PORTAL_TIMEOUT: Duration = Duration::from_millis(1500);
/// Most variant levels unwrapped from a setting.
const MAX_VARIANT_DEPTH: usize = 2;

/// Failure to read a portal setting.
#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("cannot connect to the session bus")]
    Connect(#[source] zbus::Error),
    #[error("cannot ask the session bus whether the portal is installed or runs")]
    Owner(#[source] zbus::fdo::Error),
    #[error("no desktop portal is installed or runs on the session bus")]
    NoPortal,
    /// The portal is absent, does not know the setting, or failed.
    #[error("portal call failed")]
    Call(#[source] zbus::Error),
    #[error("portal did not answer in time")]
    TimedOut(#[source] tokio::time::error::Elapsed),
    #[error("portal setting is not a string")]
    NotString,
    #[error("portal setting is not an unsigned 32-bit integer")]
    NotU32,
    #[error("portal setting is not a boolean")]
    NotBool,
}

/// A session bus connection for reading portal settings.
pub struct Portal {
    conn: Connection,
}

impl Portal {
    /// Connects to the session bus and checks that the desktop portal owns
    /// its name or can be started by the bus, within [`PORTAL_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Returns [`PortalError::Connect`] when the bus is unreachable,
    /// [`PortalError::Owner`] when it cannot tell the name's owner,
    /// [`PortalError::NoPortal`] when no portal runs or can be started by the
    /// bus, and
    /// [`PortalError::TimedOut`] when the bus does not answer in time.
    #[tracing::instrument(name = "portal_connect", skip_all)]
    pub async fn connect() -> Result<Self, PortalError> {
        timeout(PORTAL_TIMEOUT, Self::setup())
            .await
            .map_err(PortalError::TimedOut)?
    }

    async fn setup() -> Result<Self, PortalError> {
        let conn = Connection::session().await.map_err(PortalError::Connect)?;
        let bus = DBusProxy::new(&conn).await.map_err(PortalError::Connect)?;
        let name = WellKnownName::from_static_str_unchecked(DESTINATION);
        let owned = bus
            .name_has_owner(name.into())
            .await
            .map_err(PortalError::Owner)?;
        let activatable = if owned {
            false
        } else {
            bus.list_activatable_names()
                .await
                .map_err(PortalError::Owner)?
                .iter()
                .any(|listed| listed.as_str() == DESTINATION)
        };
        if !portal_available(owned, activatable) {
            return Err(PortalError::NoPortal);
        }
        Ok(Self { conn })
    }

    /// Returns the string setting `key` of `namespace` through
    /// `org.freedesktop.portal.Settings.ReadOne`, or `None` when it is empty.
    ///
    /// # Errors
    ///
    /// Returns [`PortalError::Call`] when the portal is absent, lacks
    /// `ReadOne` or the setting, [`PortalError::TimedOut`] after
    /// [`PORTAL_TIMEOUT`], and [`PortalError::NotString`] for a value of
    /// another type.
    #[tracing::instrument(name = "portal_read", skip_all, fields(namespace = %namespace, key = %key), err(level = "debug"))]
    pub async fn read_string(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<String>, PortalError> {
        let value = self.read_one(namespace, key).await?;
        string_setting(&value)
    }

    /// Returns the unsigned 32-bit setting `key` of `namespace` through
    /// `org.freedesktop.portal.Settings.ReadOne`.
    ///
    /// # Errors
    ///
    /// As [`Portal::read_string`], with [`PortalError::NotU32`] for a value
    /// of another type.
    #[tracing::instrument(name = "portal_read", skip_all, fields(namespace = %namespace, key = %key))]
    pub async fn read_u32(&self, namespace: &str, key: &str) -> Result<u32, PortalError> {
        let value = self.read_one(namespace, key).await?;
        u32_setting(&value)
    }

    /// Returns the boolean setting `key` of `namespace` through
    /// `org.freedesktop.portal.Settings.ReadOne`.
    ///
    /// # Errors
    ///
    /// As [`Portal::read_string`], with [`PortalError::NotBool`] for a value
    /// of another type.
    #[tracing::instrument(name = "portal_read", skip_all, fields(namespace = %namespace, key = %key))]
    pub async fn read_bool(&self, namespace: &str, key: &str) -> Result<bool, PortalError> {
        let value = self.read_one(namespace, key).await?;
        bool_setting(&value)
    }

    async fn read_one(&self, namespace: &str, key: &str) -> Result<OwnedVariant, PortalError> {
        let body = (namespace, key);
        let call =
            self.conn
                .call_method(Some(DESTINATION), PATH, Some(INTERFACE), "ReadOne", &body);
        let reply = timeout(PORTAL_TIMEOUT, call)
            .await
            .map_err(PortalError::TimedOut)?
            .map_err(PortalError::Call)?;
        reply.body().deserialize().map_err(PortalError::Call)
    }
}

/// Returns whether the portal can answer: it runs, or the bus starts it on
/// the first call.
fn portal_available(owned: bool, activatable: bool) -> bool {
    owned || activatable
}

/// Returns the innermost value of `value`, unwrapping at most
/// [`MAX_VARIANT_DEPTH`] nested variants.
fn unwrapped<'a>(value: &'a Variant<'a>) -> &'a Variant<'a> {
    let mut value = value;
    for _ in 0..MAX_VARIANT_DEPTH {
        match value {
            Variant::Variant(inner) => value = inner,
            _ => break,
        }
    }
    value
}

/// Returns the string held by `value`, unwrapping at most
/// [`MAX_VARIANT_DEPTH`] nested variants, or `None` for an empty string.
fn string_setting(value: &Variant<'_>) -> Result<Option<String>, PortalError> {
    match unwrapped(value) {
        Variant::Str(text) if text.is_empty() => Ok(None),
        Variant::Str(text) => Ok(Some(text.as_str().to_owned())),
        _ => Err(PortalError::NotString),
    }
}

/// Returns the `u32` held by `value`, unwrapping at most
/// [`MAX_VARIANT_DEPTH`] nested variants.
fn u32_setting(value: &Variant<'_>) -> Result<u32, PortalError> {
    match unwrapped(value) {
        Variant::U32(number) => Ok(*number),
        _ => Err(PortalError::NotU32),
    }
}

/// Returns the boolean held by `value`, unwrapping at most
/// [`MAX_VARIANT_DEPTH`] nested variants.
fn bool_setting(value: &Variant<'_>) -> Result<bool, PortalError> {
    match unwrapped(value) {
        Variant::Bool(flag) => Ok(*flag),
        _ => Err(PortalError::NotBool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_is_used_when_running_or_activatable() {
        assert!(portal_available(true, false));
        assert!(portal_available(false, true));
        assert!(!portal_available(false, false));
    }

    #[test]
    fn reply_values_become_strings() -> Result<(), PortalError> {
        assert_eq!(
            string_setting(&Variant::from("Papirus-Dark"))?,
            Some("Papirus-Dark".to_owned())
        );
        assert_eq!(string_setting(&Variant::from(""))?, None);
        assert_eq!(
            string_setting(&Variant::Variant(Box::new(Variant::from("breeze"))))?,
            Some("breeze".to_owned())
        );
        assert!(matches!(
            string_setting(&Variant::from(7_u32)),
            Err(PortalError::NotString)
        ));
        let nested = |depth: usize| {
            (0..depth).fold(Variant::from("deep"), |inner, _| {
                Variant::Variant(Box::new(inner))
            })
        };
        assert_eq!(string_setting(&nested(2))?, Some("deep".to_owned()));
        assert!(matches!(
            string_setting(&nested(3)),
            Err(PortalError::NotString)
        ));
        Ok(())
    }

    #[test]
    fn reply_values_become_numbers_and_flags() -> Result<(), PortalError> {
        assert_eq!(u32_setting(&Variant::from(1_u32))?, 1);
        assert_eq!(
            u32_setting(&Variant::Variant(Box::new(Variant::from(0_u32))))?,
            0
        );
        assert!(matches!(
            u32_setting(&Variant::from(1_i32)),
            Err(PortalError::NotU32)
        ));
        assert!(!bool_setting(&Variant::from(false))?);
        assert!(bool_setting(&Variant::Variant(Box::new(Variant::from(
            true
        ))))?);
        assert!(matches!(
            bool_setting(&Variant::from("false")),
            Err(PortalError::NotBool)
        ));
        Ok(())
    }
}
