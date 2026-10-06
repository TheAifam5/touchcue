//! Event representation shared by every IPC endpoint.

use std::collections::BTreeMap;

use serde::Serialize;
use touchcue_core::{EndReason, Event};

/// Placeholder keys published besides every `request.*` key. `app.icon` is
/// the only path among them; executable paths, uids, pids and command lines
/// stay in the daemon.
const ALLOWED: [&str; 12] = [
    "device.vendor",
    "device.model",
    "device.product",
    "device.kind",
    "device.transport",
    "device.vid",
    "device.pid",
    "app.name",
    "app.id",
    "app.icon",
    "app.container",
    "process.name",
];

fn allowed(key: &str) -> bool {
    key.starts_with("request.") || ALLOWED.contains(&key)
}

/// Change a [`WireEvent`] reports, serialized in lowercase.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Started,
    Updated,
    Ended,
}

/// A request lifecycle event as published to IPC clients.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct WireEvent {
    pub kind: Kind,
    pub id: u64,
    /// Request state name, such as `waiting` or `timed_out`.
    pub state: String,
    /// End reason in lowercase snake case; `Some` only for [`Kind::Ended`].
    pub reason: Option<String>,
    /// Detector name, such as `fido` or `gpg`.
    pub source: String,
    /// Allowlisted placeholder values: `request.*`,
    /// `device.{vendor,model,product,kind,transport,vid,pid}`,
    /// `app.{name,id,icon,container}` and `process.name`.
    pub values: BTreeMap<String, String>,
}

impl WireEvent {
    /// Builds from a machine event plus the placeholder values, keeping only
    /// the allowlisted keys.
    #[must_use]
    pub fn new(event: &Event, values: &BTreeMap<String, String>) -> WireEvent {
        let (kind, request, reason) = match event {
            Event::Started(request) => (Kind::Started, request, None),
            Event::Updated(request) => (Kind::Updated, request, None),
            Event::Ended { request, reason } => (Kind::Ended, request, Some(reason_str(*reason))),
        };
        WireEvent {
            kind,
            id: request.id.0,
            state: request.state.as_str().to_owned(),
            reason: reason.map(str::to_owned),
            source: request.source.as_str().to_owned(),
            values: values
                .iter()
                .filter(|(key, _)| allowed(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

fn reason_str(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Touched => "touched",
        EndReason::Cancelled => "cancelled",
        EndReason::Failed => "failed",
        EndReason::TimedOut => "timed_out",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use touchcue_core::{
        Device, DeviceId, DeviceKind, Method, Request, RequestId, RequestState, SignalClass,
        Source, Transport,
    };

    use super::*;

    pub(crate) fn request(id: u64, source: Source, state: RequestState) -> Request {
        Request {
            id: RequestId(id),
            device: Device {
                id: DeviceId("hidraw0".to_owned()),
                kind: DeviceKind::Fido,
                transport: Transport::Usb,
                vid: Some(0x1050),
                pid: Some(0x0407),
                vendor: None,
                model: None,
                product: None,
            },
            source,
            class: SignalClass::Asserted,
            method: Method::Fido2,
            op: None,
            channel: None,
            pids: Vec::new(),
            started: Instant::now(),
            count: 1,
            detail: None,
            state,
        }
    }

    fn values() -> BTreeMap<String, String> {
        [
            ("app.cmdline", "ssh -i secret host"),
            ("app.exe", "/usr/bin/ssh"),
            ("app.name", "ssh"),
            ("app.pid", "41"),
            ("app.wm_class", "foot"),
            ("device.pid", "0407"),
            ("process.cmdline", "gpg --passphrase x"),
            ("process.exe", "/usr/bin/gpg"),
            ("process.name", "gpg"),
            ("process.pid", "42"),
            ("process.uid", "1000"),
            ("request.state", "waiting"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    #[test]
    fn new_keeps_only_allowlisted_keys() {
        let event = Event::Started(request(1, Source::Fido, RequestState::Waiting));
        let wire = WireEvent::new(&event, &values());
        assert_eq!(
            wire.values.keys().map(String::as_str).collect::<Vec<_>>(),
            ["app.name", "device.pid", "process.name", "request.state"]
        );
    }

    #[test]
    fn started_serializes_to_exact_json() -> Result<(), serde_json::Error> {
        let event = Event::Started(request(7, Source::Gpg, RequestState::Waiting));
        let json = serde_json::to_string(&WireEvent::new(&event, &values()))?;
        assert_eq!(
            json,
            r#"{"kind":"started","id":7,"state":"waiting","reason":null,"source":"gpg","values":{"app.name":"ssh","device.pid":"0407","process.name":"gpg","request.state":"waiting"}}"#
        );
        Ok(())
    }

    #[test]
    fn ended_carries_snake_case_reason() -> Result<(), serde_json::Error> {
        let event = Event::Ended {
            request: request(
                3,
                Source::Hmac,
                RequestState::Lingering(EndReason::TimedOut),
            ),
            reason: EndReason::TimedOut,
        };
        let json = serde_json::to_string(&WireEvent::new(&event, &BTreeMap::new()))?;
        assert_eq!(
            json,
            r#"{"kind":"ended","id":3,"state":"timed_out","reason":"timed_out","source":"hmac","values":{}}"#
        );
        Ok(())
    }
}
