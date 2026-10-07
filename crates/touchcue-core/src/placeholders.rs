//! Placeholder keys available to templates and the values bound to them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::machine::Request;
use crate::model::Device;
use crate::text::sanitize;

/// Every key a template may reference.
pub const KNOWN: &[&str] = &[
    "app.name",
    "app.id",
    "app.icon",
    "app.exe",
    "app.pid",
    "app.cmdline",
    "app.wm_class",
    "app.container",
    "process.name",
    "process.exe",
    "process.pid",
    "process.cmdline",
    "process.uid",
    "process.chain",
    "requester.name",
    "requester.exe",
    "requester.pid",
    "requester.label",
    "device.vendor",
    "device.model",
    "device.product",
    "device.vid",
    "device.pid",
    "device.kind",
    "device.transport",
    "request.method",
    "request.op",
    "request.source",
    "request.class",
    "request.confidence",
    "request.elapsed",
    "request.count",
    "request.state",
    "request.detail",
];

/// Keys published outside the daemon besides every `request.*` key.
/// `app.icon` is the only path among them; executable paths, uids, pids and
/// command lines stay in the daemon.
pub const PUBLISHED: [&str; 15] = [
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
    "process.chain",
    "requester.name",
    "requester.label",
];

/// Returns whether the value of `key` may leave the daemon: every
/// `request.*` key and the keys in [`PUBLISHED`].
#[must_use]
pub fn published(key: &str) -> bool {
    key.starts_with("request.") || PUBLISHED.contains(&key)
}

/// Application a request is attributed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppInfo {
    pub name: Option<String>,
    pub id: Option<String>,
    pub icon: Option<String>,
    pub exe: Option<PathBuf>,
    pub pid: Option<u32>,
    pub cmdline: Option<String>,
    pub wm_class: Option<String>,
    pub container: Option<String>,
}

/// Client process that talks to the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub name: Option<String>,
    pub exe: Option<PathBuf>,
    pub pid: u32,
    pub cmdline: Option<String>,
    pub uid: Option<u32>,
}

/// Process a request is attributed to as its requester: the program started
/// inside the application that led to the request, as chosen by
/// [`skip::requester`](crate::skip::requester).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester {
    pub name: Option<String>,
    pub exe: Option<PathBuf>,
    pub pid: u32,
}

/// How reliably a request is attributed to its application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    High,
    /// The client is inferred from processes connected to an agent, not
    /// from the device itself, or is one of several processes holding the
    /// device.
    Medium,
    Low,
}

impl Confidence {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// Longest `request.detail`, in chars.
const DETAIL_MAX: usize = 200;

/// Returns the placeholder values for a request, keyed by entries of [`KNOWN`].
///
/// Absent values are omitted. `chain` is the value of `process.chain`, and
/// `requester.label` is composed from `requester.name` and `app.name`.
/// `request.detail` is sanitized and capped at
/// 200 chars. `device.vid` and `device.pid` are four-digit
/// lowercase hex, `request.elapsed` is whole seconds since the request
/// started, or 0 when `now` is earlier, and `request.state` is
/// [`RequestState::as_str`](crate::machine::RequestState::as_str).
#[must_use]
pub fn values(
    request: &Request,
    app: Option<&AppInfo>,
    process: Option<&ProcessInfo>,
    requester: Option<&Requester>,
    chain: Option<&str>,
    confidence: Confidence,
    now: Instant,
) -> BTreeMap<String, String> {
    let mut out = device_values(&request.device);
    let mut put = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            out.insert(key.to_owned(), value);
        }
    };

    if let Some(app) = app {
        put("app.name", app.name.clone());
        put("app.id", app.id.clone());
        put("app.icon", app.icon.clone());
        put("app.exe", app.exe.as_ref().map(|p| p.display().to_string()));
        put("app.pid", app.pid.map(|p| p.to_string()));
        put("app.cmdline", app.cmdline.clone());
        put("app.wm_class", app.wm_class.clone());
        put("app.container", app.container.clone());
    }
    if let Some(process) = process {
        put("process.name", process.name.clone());
        put(
            "process.exe",
            process.exe.as_ref().map(|p| p.display().to_string()),
        );
        put("process.pid", Some(process.pid.to_string()));
        put("process.cmdline", process.cmdline.clone());
        put("process.uid", process.uid.map(|u| u.to_string()));
    }
    put("process.chain", chain.map(str::to_owned));
    if let Some(requester) = requester {
        put("requester.name", requester.name.clone());
        put(
            "requester.exe",
            requester.exe.as_ref().map(|p| p.display().to_string()),
        );
        put("requester.pid", Some(requester.pid.to_string()));
    }
    put(
        "requester.label",
        requester_label(
            requester.and_then(|r| r.name.as_deref()),
            app.and_then(|a| a.name.as_deref()),
        ),
    );

    put("request.method", Some(request.method.as_str().to_owned()));
    put("request.op", request.op.map(|op| op.as_str().to_owned()));
    put("request.source", Some(request.source.as_str().to_owned()));
    put("request.class", Some(request.class.as_str().to_owned()));
    put("request.confidence", Some(confidence.as_str().to_owned()));
    let elapsed = now.saturating_duration_since(request.started).as_secs();
    put("request.elapsed", Some(elapsed.to_string()));
    put("request.count", Some(request.count.to_string()));
    put("request.state", Some(request.state.as_str().to_owned()));
    put(
        "request.detail",
        request
            .detail
            .as_deref()
            .and_then(|detail| sanitize(detail, DETAIL_MAX)),
    );
    out
}

/// Longest `requester.label`, in chars: two 128-char names and ` in `.
const LABEL_MAX: usize = 260;

/// Returns `requester.label`: `<requester> in <app>` when both names are
/// present and differ other than in ASCII case, else whichever is present,
/// preferring the application's; capped at 260 chars.
#[must_use]
pub fn requester_label(requester: Option<&str>, app: Option<&str>) -> Option<String> {
    let requester = requester.filter(|name| !name.is_empty());
    let app = app.filter(|name| !name.is_empty());
    let label = match (requester, app) {
        (Some(requester), Some(app)) if !requester.eq_ignore_ascii_case(app) => {
            format!("{requester} in {app}")
        }
        (_, Some(name)) | (Some(name), None) => name.to_owned(),
        (None, None) => return None,
    };
    Some(label.chars().take(LABEL_MAX).collect())
}

/// Returns the `device.*` placeholder values of `device`, formatted as in
/// [`values`]; absent values are omitted.
#[must_use]
pub fn device_values(device: &Device) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut put = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            out.insert(key.to_owned(), value);
        }
    };
    put("device.vendor", device.vendor.clone());
    put("device.model", device.model.clone());
    put("device.product", device.product.clone());
    put("device.vid", device.vid.map(|v| format!("{v:04x}")));
    put("device.pid", device.pid.map(|p| format!("{p:04x}")));
    put("device.kind", Some(device.kind.as_str().to_owned()));
    put(
        "device.transport",
        Some(device.transport.as_str().to_owned()),
    );
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::machine::{EndReason, RequestId, RequestState};
    use crate::model::{Device, DeviceId, DeviceKind, Method, Op, SignalClass, Source, Transport};

    fn request(started: Instant) -> Request {
        Request {
            id: RequestId(1),
            device: Device {
                id: DeviceId("yk".to_owned()),
                kind: DeviceKind::Fido,
                transport: Transport::Usb,
                vid: Some(0x1050),
                pid: Some(0x7),
                vendor: Some("Yubico".to_owned()),
                model: None,
                product: None,
            },
            source: Source::Fido,
            class: SignalClass::Disappearance,
            method: Method::Fido2,
            op: Some(Op::Assert),
            channel: Some(1),
            pids: vec![42],
            started,
            count: 2,
            detail: Some("SHA256:abc\u{1b}[2J to host".to_owned()),
            state: RequestState::Waiting,
        }
    }

    fn get<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
        map.get(key).map(String::as_str)
    }

    #[test]
    fn formats_request_and_device() {
        let base = Instant::now();
        let v = values(
            &request(base),
            None,
            None,
            None,
            None,
            Confidence::Low,
            base + Duration::from_millis(2999),
        );
        assert_eq!(get(&v, "device.vid"), Some("1050"));
        assert_eq!(get(&v, "device.pid"), Some("0007"));
        assert_eq!(get(&v, "device.kind"), Some("fido"));
        assert_eq!(get(&v, "device.transport"), Some("usb"));
        assert_eq!(get(&v, "request.elapsed"), Some("2"));
        assert_eq!(get(&v, "request.detail"), Some("SHA256:abc [2J to host"));
        assert_eq!(get(&v, "request.class"), Some("disappearance"));
        assert_eq!(get(&v, "request.method"), Some("fido2"));
        assert_eq!(get(&v, "request.op"), Some("assert"));
        assert_eq!(get(&v, "request.count"), Some("2"));
        assert_eq!(get(&v, "request.confidence"), Some("low"));
        assert_eq!(get(&v, "request.state"), Some("waiting"));
        assert_eq!(get(&v, "device.model"), None);
        assert!(!v.keys().any(|k| {
            ["app.", "process.", "requester."]
                .iter()
                .any(|p| k.starts_with(p))
        }));
        assert!(v.keys().all(|k| KNOWN.contains(&k.as_str())));
    }

    #[test]
    fn includes_present_app_and_process_fields() {
        let base = Instant::now();
        let app = AppInfo {
            name: Some("Firefox".to_owned()),
            exe: Some(PathBuf::from("/usr/bin/firefox")),
            pid: Some(10),
            ..AppInfo::default()
        };
        let process = ProcessInfo {
            name: Some("firefox".to_owned()),
            exe: None,
            pid: 11,
            cmdline: None,
            uid: Some(1000),
        };
        let v = values(
            &request(base),
            Some(&app),
            Some(&process),
            None,
            None,
            Confidence::High,
            base,
        );
        assert_eq!(get(&v, "app.name"), Some("Firefox"));
        assert_eq!(get(&v, "app.exe"), Some("/usr/bin/firefox"));
        assert_eq!(get(&v, "app.pid"), Some("10"));
        assert_eq!(get(&v, "app.icon"), None);
        assert_eq!(get(&v, "process.pid"), Some("11"));
        assert_eq!(get(&v, "process.uid"), Some("1000"));
        assert_eq!(get(&v, "process.exe"), None);
        assert_eq!(get(&v, "request.elapsed"), Some("0"));
        assert!(v.keys().all(|k| KNOWN.contains(&k.as_str())));
    }

    #[test]
    fn includes_requester_and_chain() {
        let base = Instant::now();
        let requester = Requester {
            name: Some("claude".to_owned()),
            exe: Some(PathBuf::from("/opt/claude/claude")),
            pid: 12,
        };
        let chain = "gpg ← bash ← claude";
        let v = values(
            &request(base),
            None,
            None,
            Some(&requester),
            Some(chain),
            Confidence::Medium,
            base,
        );
        assert_eq!(get(&v, "requester.name"), Some("claude"));
        assert_eq!(get(&v, "requester.exe"), Some("/opt/claude/claude"));
        assert_eq!(get(&v, "requester.pid"), Some("12"));
        assert_eq!(get(&v, "process.chain"), Some(chain));
        assert!(v.keys().all(|k| KNOWN.contains(&k.as_str())));
        let identity: Vec<&str> = v
            .keys()
            .map(String::as_str)
            .filter(|&k| published(k) && !k.starts_with("request.") && !k.starts_with("device."))
            .collect();
        assert_eq!(
            identity,
            ["process.chain", "requester.label", "requester.name"]
        );
    }

    #[test]
    fn label_names_requester_in_app() {
        let base = Instant::now();
        let app = |name: &str| AppInfo {
            name: Some(name.to_owned()),
            ..AppInfo::default()
        };
        let requester = |name: &str| Requester {
            name: Some(name.to_owned()),
            exe: None,
            pid: 7,
        };
        let label = |app: Option<&AppInfo>, requester: Option<&Requester>| {
            let v = values(
                &request(base),
                app,
                None,
                requester,
                None,
                Confidence::High,
                base,
            );
            v.get("requester.label").cloned()
        };
        let kitty = app("Kitty");
        let claude = requester("claude");
        let nu = requester("nu");
        let firefox = app("Firefox");
        let backup = requester("backup.sh");
        assert_eq!(
            label(Some(&kitty), Some(&claude)).as_deref(),
            Some("claude in Kitty")
        );
        assert_eq!(
            label(Some(&kitty), Some(&nu)).as_deref(),
            Some("nu in Kitty")
        );
        assert_eq!(label(Some(&firefox), None).as_deref(), Some("Firefox"));
        assert_eq!(label(None, Some(&backup)).as_deref(), Some("backup.sh"));
        assert_eq!(label(None, None), None);
        assert_eq!(
            label(Some(&firefox), Some(&requester("firefox"))).as_deref(),
            Some("Firefox")
        );
        let long = "x".repeat(128);
        let bounded = label(Some(&app(&long)), Some(&requester(&"y".repeat(128))))
            .map(|label| label.chars().count());
        assert_eq!(bounded, Some(LABEL_MAX));
        let huge = label(Some(&app(&"x".repeat(400))), Some(&requester("y")))
            .map(|label| label.chars().count());
        assert_eq!(huge, Some(LABEL_MAX));
    }

    #[test]
    fn elapsed_before_start_is_zero() {
        let now = Instant::now();
        let base = now + Duration::from_secs(3);
        let v = values(
            &request(base),
            None,
            None,
            None,
            None,
            Confidence::High,
            now,
        );
        assert_eq!(get(&v, "request.elapsed"), Some("0"));
    }

    #[test]
    fn state_names() {
        let base = Instant::now();
        let mut r = request(base);
        for (state, name) in [
            (RequestState::Waiting, "waiting"),
            (RequestState::Lingering(EndReason::Cancelled), "cancelled"),
            (RequestState::Lingering(EndReason::Failed), "failed"),
            (RequestState::Lingering(EndReason::TimedOut), "timed_out"),
        ] {
            r.state = state;
            let v = values(&r, None, None, None, None, Confidence::High, base);
            assert_eq!(get(&v, "request.state"), Some(name));
        }
    }
}
