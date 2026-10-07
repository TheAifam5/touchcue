//! Runs the `[[hooks]]` commands of the configuration on touchcue events.
//!
//! Commands run without a shell. Placeholder values reach them only through
//! environment variables and, for hooks with `until`, stdin, never through
//! arguments. Running commands is supported on Linux; elsewhere every run
//! fails with a logged error.

use std::collections::BTreeMap;

use touchcue_core::placeholders::published;
use touchcue_core::text::sanitize;
use touchcue_core::{EndReason, Event, HookEvent, RequestId, RequestState};

mod lifetime;
#[cfg(target_os = "linux")]
mod linux;
mod queue;
mod runner;
#[cfg(not(target_os = "linux"))]
mod stub;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(not(target_os = "linux"))]
use stub as platform;

pub use lifetime::{BODY_VAR, LIFETIME_PROCESSES, PromptText, STDIN_QUEUE, TITLE_VAR};
pub use platform::RunError;
pub use runner::{HookSender, Hooks, HooksError, KILL_GRACE, QUEUE};

/// Name of the variable holding the event name.
pub const EVENT_VAR: &str = "TOUCHCUE_EVENT";
/// Start of every variable touchcue sets for a hook; inherited variables
/// with this prefix are removed.
pub const VAR_PREFIX: &str = "TOUCHCUE_";
/// Longest value of a variable, in chars.
pub const VALUE_MAX: usize = 1024;

/// Turns request events into hook events, remembering the last state of
/// each active request.
#[derive(Debug, Default)]
pub struct RequestEvents {
    states: BTreeMap<RequestId, RequestState>,
}

impl RequestEvents {
    /// Returns the hook events of `event`, in the order documented on
    /// [`HookEvent`]. Forgets a request when it ends.
    pub fn map(&mut self, event: &Event) -> Vec<HookEvent> {
        match event {
            Event::Started(request) => {
                self.states.insert(request.id, request.state);
                let mut out = vec![HookEvent::Started];
                out.extend(entered(None, request.state));
                out
            }
            Event::Updated(request) => {
                let previous = self.states.insert(request.id, request.state);
                let mut out = vec![HookEvent::Updated];
                out.extend(entered(previous, request.state));
                out
            }
            Event::Ended { request, reason } => {
                let previous = self.states.remove(&request.id);
                let mut out = vec![HookEvent::Ended];
                if previous != Some(RequestState::Lingering(*reason)) {
                    out.push(outcome(*reason));
                }
                out
            }
        }
    }
}

/// Returns the events of a request entering `state` from `previous`.
fn entered(previous: Option<RequestState>, state: RequestState) -> Vec<HookEvent> {
    if previous == Some(state) {
        return Vec::new();
    }
    let was_lingering = matches!(previous, Some(RequestState::Lingering(_)));
    match state {
        RequestState::Waiting if was_lingering => vec![HookEvent::Revived, HookEvent::Waiting],
        RequestState::Waiting => vec![HookEvent::Waiting],
        RequestState::Lingering(reason) if was_lingering => vec![outcome(reason)],
        RequestState::Lingering(reason) => vec![HookEvent::Lingering, outcome(reason)],
    }
}

fn outcome(reason: EndReason) -> HookEvent {
    match reason {
        EndReason::Touched => HookEvent::Touched,
        EndReason::Cancelled => HookEvent::Cancelled,
        EndReason::Failed => HookEvent::Failed,
        EndReason::TimedOut => HookEvent::TimedOut,
    }
}

/// Returns the environment variables of a hook run for `event`.
///
/// [`EVENT_VAR`] holds the event name. Each published placeholder becomes
/// `TOUCHCUE_` plus its key uppercased with dots replaced by underscores,
/// such as `TOUCHCUE_APP_NAME`, holding the value sanitized and capped at
/// [`VALUE_MAX`] chars; a value with nothing visible is omitted. Keys that
/// are not published, such as command lines and executable paths, are left
/// out.
#[must_use]
pub fn env(event: HookEvent, values: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = vec![(EVENT_VAR.to_owned(), event.as_str().to_owned())];
    out.extend(vars(&published_values(values)));
    out
}

/// Returns the published entries of `values`, sanitized and capped at
/// [`VALUE_MAX`] chars, without the ones with nothing visible.
fn published_values(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .filter(|(key, _)| published(key))
        .filter_map(|(key, value)| Some((key.clone(), sanitize(value, VALUE_MAX)?)))
        .collect()
}

/// Returns the variables of published and sanitized `values`.
fn vars(values: &BTreeMap<String, String>) -> impl Iterator<Item = (String, String)> {
    values
        .iter()
        .map(|(key, value)| (var_name(key), value.clone()))
}

fn var_name(key: &str) -> String {
    format!("{VAR_PREFIX}{}", key.replace('.', "_").to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use touchcue_core::{
        Device, DeviceId, DeviceKind, Method, Request, SignalClass, Source, Transport,
    };

    use super::*;

    fn request(state: RequestState) -> Request {
        Request {
            id: RequestId(1),
            device: Device {
                id: DeviceId("/dev/hidraw0".to_owned()),
                kind: DeviceKind::Fido,
                transport: Transport::Usb,
                vid: None,
                pid: None,
                vendor: None,
                model: None,
                product: None,
            },
            source: Source::Fido,
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

    fn lingering(reason: EndReason) -> RequestState {
        RequestState::Lingering(reason)
    }

    #[test]
    fn touch_lifecycle_maps_to_events() {
        use HookEvent::{Ended, Started, Touched, Updated, Waiting};

        let mut map = RequestEvents::default();
        assert_eq!(
            map.map(&Event::Started(request(RequestState::Waiting))),
            [Started, Waiting]
        );
        assert_eq!(
            map.map(&Event::Updated(request(RequestState::Waiting))),
            [Updated]
        );
        let ended = Event::Ended {
            request: request(lingering(EndReason::Touched)),
            reason: EndReason::Touched,
        };
        assert_eq!(map.map(&ended), [Ended, Touched]);
        assert!(map.states.is_empty());
    }

    #[test]
    fn lingering_revival_and_outcomes_fire_once() {
        use HookEvent::{Cancelled, Ended, Failed, Lingering, Revived, TimedOut, Updated, Waiting};

        let mut map = RequestEvents::default();
        map.map(&Event::Started(request(RequestState::Waiting)));
        assert_eq!(
            map.map(&Event::Updated(request(lingering(EndReason::Cancelled)))),
            [Updated, Lingering, Cancelled]
        );
        assert_eq!(
            map.map(&Event::Updated(request(lingering(EndReason::Cancelled)))),
            [Updated]
        );
        assert_eq!(
            map.map(&Event::Updated(request(lingering(EndReason::Failed)))),
            [Updated, Failed]
        );
        assert_eq!(
            map.map(&Event::Updated(request(RequestState::Waiting))),
            [Updated, Revived, Waiting]
        );
        assert_eq!(
            map.map(&Event::Updated(request(lingering(EndReason::TimedOut)))),
            [Updated, Lingering, TimedOut]
        );
        let ended = Event::Ended {
            request: request(lingering(EndReason::TimedOut)),
            reason: EndReason::TimedOut,
        };
        assert_eq!(map.map(&ended), [Ended]);
        assert!(map.states.is_empty());
    }

    #[test]
    fn env_keeps_published_keys_sanitized() {
        let values: BTreeMap<String, String> = [
            ("app.name", "Fire\u{202E}fox\n"),
            ("app.exe", "/usr/bin/firefox"),
            ("app.cmdline", "firefox --secret"),
            ("process.name", "firefox"),
            ("process.pid", "42"),
            ("device.vid", "1050"),
            ("request.state", "waiting"),
            ("request.detail", "\u{200B}"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let env = env(HookEvent::Started, &values);
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("TOUCHCUE_EVENT", "started"),
                ("TOUCHCUE_APP_NAME", "Fire fox"),
                ("TOUCHCUE_DEVICE_VID", "1050"),
                ("TOUCHCUE_PROCESS_NAME", "firefox"),
                ("TOUCHCUE_REQUEST_STATE", "waiting"),
            ]
        );
    }

    #[test]
    fn env_caps_values() {
        let values = BTreeMap::from([("app.name".to_owned(), "x".repeat(5000))]);
        let env = env(HookEvent::DeviceAdded, &values);
        let name = env.iter().find(|(k, _)| k == "TOUCHCUE_APP_NAME");
        assert_eq!(name.map(|(_, v)| v.chars().count()), Some(VALUE_MAX));
        assert_eq!(
            env.first().map(|(k, v)| (k.as_str(), v.as_str())),
            Some(("TOUCHCUE_EVENT", "device_added"))
        );
    }
}
