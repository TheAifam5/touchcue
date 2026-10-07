//! Desktop settings read through the XDG desktop portal.

use std::time::Duration;

use tokio::time::timeout;
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::names::WellKnownName;
use zbus::zvariant::{OwnedValue, Value};

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
    #[tracing::instrument(name = "portal_connect", skip_all, err)]
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
    #[tracing::instrument(name = "portal_read", skip_all, fields(namespace = %namespace, key = %key), err)]
    pub async fn read_string(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<String>, PortalError> {
        let body = (namespace, key);
        let call =
            self.conn
                .call_method(Some(DESTINATION), PATH, Some(INTERFACE), "ReadOne", &body);
        let reply = timeout(PORTAL_TIMEOUT, call)
            .await
            .map_err(PortalError::TimedOut)?
            .map_err(PortalError::Call)?;
        let value: OwnedValue = reply.body().deserialize().map_err(PortalError::Call)?;
        string_setting(&value)
    }
}

/// Returns whether the portal can answer: it runs, or the bus starts it on
/// the first call.
fn portal_available(owned: bool, activatable: bool) -> bool {
    owned || activatable
}

/// Returns the string held by `value`, unwrapping at most
/// [`MAX_VARIANT_DEPTH`] nested variants, or `None` for an empty string.
fn string_setting(value: &Value<'_>) -> Result<Option<String>, PortalError> {
    let mut value = value;
    for _ in 0..MAX_VARIANT_DEPTH {
        match value {
            Value::Value(inner) => value = inner,
            _ => break,
        }
    }
    match value {
        Value::Str(text) if text.is_empty() => Ok(None),
        Value::Str(text) => Ok(Some(text.as_str().to_owned())),
        _ => Err(PortalError::NotString),
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
            string_setting(&Value::from("Papirus-Dark"))?,
            Some("Papirus-Dark".to_owned())
        );
        assert_eq!(string_setting(&Value::from(""))?, None);
        assert_eq!(
            string_setting(&Value::Value(Box::new(Value::from("breeze"))))?,
            Some("breeze".to_owned())
        );
        assert!(matches!(
            string_setting(&Value::from(7_u32)),
            Err(PortalError::NotString)
        ));
        let nested = |depth: usize| {
            (0..depth).fold(Value::from("deep"), |inner, _| {
                Value::Value(Box::new(inner))
            })
        };
        assert_eq!(string_setting(&nested(2))?, Some("deep".to_owned()));
        assert!(matches!(
            string_setting(&nested(3)),
            Err(PortalError::NotString)
        ));
        Ok(())
    }
}
