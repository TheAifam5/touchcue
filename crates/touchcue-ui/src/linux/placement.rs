//! Which outputs show a popup and where popups stack on them.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use touchcue_core::config::{OutputTarget, Popup, Position};
use tracing::instrument;

use super::modal::dim_alpha;

/// Longest time one Hyprland IPC request may take, connect to close.
const HYPRLAND_TIMEOUT: Duration = Duration::from_millis(500);
/// Largest Hyprland IPC reply read, in bytes.
const MAX_HYPRLAND_REPLY: u64 = 64 * 1024;

/// Where popups are shown and how modal overlays behave.
#[derive(Debug, Clone)]
pub(crate) struct Placement {
    pub(crate) position: Position,
    pub(crate) target: OutputTarget,
    /// Alpha of the black overlay drawn behind modal popups.
    pub(crate) dim_alpha: u8,
    /// Popups of waiting requests may hold overlays.
    pub(crate) modal: bool,
    /// A click on an overlay dismisses the requests that hold it.
    pub(crate) dismiss: bool,
}

impl Placement {
    pub(crate) fn new(popup: &Popup) -> Self {
        Self {
            position: popup.position,
            target: popup.output.clone(),
            dim_alpha: dim_alpha(popup.modal_dim.get()),
            modal: popup.modal,
            dismiss: popup.modal && popup.modal_dismiss,
        }
    }
}

/// Returns the distance of each popup from the top of a stack of popups
/// `heights` tall with `spacing` between them, and the height of the stack.
pub(crate) fn stack(heights: &[i32], spacing: i32) -> (Vec<i32>, i32) {
    let mut offsets = Vec::with_capacity(heights.len());
    let mut total: i32 = 0;
    for (index, height) in heights.iter().enumerate() {
        if index > 0 {
            total = total.saturating_add(spacing);
        }
        offsets.push(total);
        total = total.saturating_add(*height);
    }
    (offsets, total)
}

/// Returns the indices of the `available` outputs named in `wanted`, in
/// `available` order; outputs without a name never match.
pub(crate) fn matching(wanted: &[String], available: &[Option<&str>]) -> Vec<usize> {
    available
        .iter()
        .enumerate()
        .filter(|(_, name)| name.is_some_and(|name| wanted.iter().any(|w| w == name)))
        .map(|(index, _)| index)
        .collect()
}

/// Why the output under the cursor is unknown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HyprlandError {
    #[error("not running under Hyprland")]
    NotHyprland,
    #[error("HYPRLAND_INSTANCE_SIGNATURE is not a single path component")]
    Signature,
    #[error("XDG_RUNTIME_DIR is not an absolute path")]
    RuntimeDir,
    #[error("Hyprland IPC failed")]
    Io(#[from] io::Error),
    #[error("Hyprland IPC took longer than {HYPRLAND_TIMEOUT:?}")]
    Timeout(#[source] tokio::time::error::Elapsed),
    #[error("Hyprland IPC reply is larger than {MAX_HYPRLAND_REPLY} bytes")]
    TooLarge,
    /// Keeps only the position of the error, so reply text never reaches
    /// logs.
    #[error("Hyprland IPC reply is not the expected JSON ({category:?} error at {line}:{column})")]
    Json {
        category: serde_json::error::Category,
        line: usize,
        column: usize,
    },
}

impl From<serde_json::Error> for HyprlandError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json {
            category: err.classify(),
            line: err.line(),
            column: err.column(),
        }
    }
}

/// One entry of the Hyprland `j/monitors` reply.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct HyprMonitor {
    pub(crate) name: String,
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// Mode width in device pixels, before scale and rotation.
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) scale: f64,
    /// `wl_output` transform; odd values rotate by 90 or 270 degrees.
    pub(crate) transform: u8,
    #[serde(default)]
    pub(crate) disabled: bool,
}

impl HyprMonitor {
    /// Reports whether `(x, y)` is inside the monitor's logical box, which
    /// includes its top and left edges.
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        if !(self.scale.is_finite() && self.scale > 0.0) {
            return false;
        }
        let (mut width, mut height) = (self.width / self.scale, self.height / self.scale);
        if self.transform % 2 == 1 {
            (width, height) = (height, width);
        }
        x >= self.x && x < self.x + width && y >= self.y && y < self.y + height
    }
}

/// The Hyprland `j/cursorpos` reply, in global logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct HyprCursor {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// Returns the name of the enabled monitor that contains `cursor`.
pub(crate) fn monitor_at(monitors: &[HyprMonitor], cursor: HyprCursor) -> Option<&str> {
    monitors
        .iter()
        .find(|monitor| !monitor.disabled && monitor.contains((cursor.x, cursor.y)))
        .map(|monitor| monitor.name.as_str())
}

/// Returns the socket of the Hyprland instance named by the environment.
fn hyprland_socket(
    runtime_dir: Option<OsString>,
    signature: Option<OsString>,
) -> Result<PathBuf, HyprlandError> {
    let (Some(runtime_dir), Some(signature)) = (runtime_dir, signature) else {
        return Err(HyprlandError::NotHyprland);
    };
    if runtime_dir.is_empty() || signature.is_empty() {
        return Err(HyprlandError::NotHyprland);
    }
    if !std::path::Path::new(&runtime_dir).is_absolute() {
        return Err(HyprlandError::RuntimeDir);
    }
    let mut components = std::path::Path::new(&signature).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => {}
        _ => return Err(HyprlandError::Signature),
    }
    Ok(PathBuf::from(runtime_dir)
        .join("hypr")
        .join(signature)
        .join(".socket.sock"))
}

/// Returns the name of the Hyprland monitor under the cursor, or `None`
/// when the cursor is outside every enabled monitor.
///
/// Each of the two requests uses its own connection and fails after
/// [`HYPRLAND_TIMEOUT`].
#[instrument(skip_all)]
pub(crate) async fn hyprland_cursor_output() -> Result<Option<String>, HyprlandError> {
    let socket = hyprland_socket(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"),
    )?;
    let monitors: Vec<HyprMonitor> =
        serde_json::from_slice(&hyprland_request(&socket, b"j/monitors").await?)?;
    let cursor: HyprCursor =
        serde_json::from_slice(&hyprland_request(&socket, b"j/cursorpos").await?)?;
    Ok(monitor_at(&monitors, cursor).map(str::to_owned))
}

/// Sends `command` on a new connection and reads the reply until the
/// compositor closes it.
async fn hyprland_request(socket: &PathBuf, command: &[u8]) -> Result<Vec<u8>, HyprlandError> {
    let reply = tokio::time::timeout(HYPRLAND_TIMEOUT, async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(command).await?;
        let mut reply = Vec::new();
        (&mut stream)
            .take(MAX_HYPRLAND_REPLY + 1)
            .read_to_end(&mut reply)
            .await?;
        Ok::<_, io::Error>(reply)
    })
    .await
    .map_err(HyprlandError::Timeout)??;
    match u64::try_from(reply.len()) {
        Ok(len) if len <= MAX_HYPRLAND_REPLY => Ok(reply),
        Ok(_) | Err(_) => Err(HyprlandError::TooLarge),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `j/monitors` and `j/cursorpos` recorded on a four-monitor Hyprland
    /// desktop; HDMI-A-1 is rotated, so DP-1 starts at its logical width.
    const MONITORS: &str = r#"[{"name":"HDMI-A-1","x":0,"y":0,"width":1920,"height":1080,"scale":1,"transform":1,"focused":false,"disabled":false},
 {"name":"DP-1","x":1080,"y":0,"width":1920,"height":1080,"scale":1,"transform":0,"focused":true,"disabled":false},
 {"name":"DP-2","x":3000,"y":0,"width":1920,"height":1080,"scale":1,"transform":0,"focused":false,"disabled":false},
 {"name":"DP-3","x":4920,"y":0,"width":1920,"height":1080,"scale":1,"transform":3,"focused":false,"disabled":false}]"#;
    const CURSOR: &str = r#"{"x": 1987, "y": 708}"#;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Json(#[from] serde_json::Error),
    }

    fn at(monitors: &[HyprMonitor], x: f64, y: f64) -> Option<&str> {
        monitor_at(monitors, HyprCursor { x, y })
    }

    #[test]
    fn recorded_cursor_maps_to_its_monitor() -> Result<(), TestError> {
        let monitors: Vec<HyprMonitor> = serde_json::from_str(MONITORS)?;
        let cursor: HyprCursor = serde_json::from_str(CURSOR)?;
        assert_eq!(monitors.len(), 4);
        assert_eq!(monitor_at(&monitors, cursor), Some("DP-1"));
        Ok(())
    }

    #[test]
    fn rotation_and_boundaries() -> Result<(), TestError> {
        let monitors: Vec<HyprMonitor> = serde_json::from_str(MONITORS)?;
        assert_eq!(at(&monitors, 1079.0, 708.0), Some("HDMI-A-1"));
        assert_eq!(at(&monitors, 1080.0, 708.0), Some("DP-1"));
        // The rotated monitor is 1920 logical pixels tall.
        assert_eq!(at(&monitors, 500.0, 1500.0), Some("HDMI-A-1"));
        assert_eq!(at(&monitors, 500.0, 1920.0), None);
        assert_eq!(at(&monitors, 1500.0, 1080.0), None);
        assert_eq!(at(&monitors, 2999.5, 0.0), Some("DP-1"));
        assert_eq!(at(&monitors, 3000.0, 0.0), Some("DP-2"));
        assert_eq!(at(&monitors, 5999.0, 1900.0), Some("DP-3"));
        assert_eq!(at(&monitors, 6000.0, 0.0), None);
        assert_eq!(at(&monitors, -1.0, 0.0), None);
        Ok(())
    }

    #[test]
    fn scale_shrinks_the_logical_box() -> Result<(), TestError> {
        let monitors: Vec<HyprMonitor> = serde_json::from_str(
            r#"[{"name":"eDP-1","x":0,"y":0,"width":2880,"height":1800,"scale":2,"transform":0},
                {"name":"DP-9","x":1440,"y":0,"width":1920,"height":1080,"scale":1.5,"transform":0}]"#,
        )?;
        assert_eq!(at(&monitors, 1439.0, 899.0), Some("eDP-1"));
        assert_eq!(at(&monitors, 1440.0, 100.0), Some("DP-9"));
        assert_eq!(at(&monitors, 2719.0, 719.0), Some("DP-9"));
        assert_eq!(at(&monitors, 2720.0, 100.0), None);
        Ok(())
    }

    #[test]
    fn disabled_monitors_are_skipped() -> Result<(), TestError> {
        let mut monitors: Vec<HyprMonitor> = serde_json::from_str(MONITORS)?;
        for monitor in &mut monitors {
            monitor.disabled = monitor.name == "DP-1";
        }
        assert_eq!(at(&monitors, 1987.0, 708.0), None);
        assert_eq!(at(&monitors, 3001.0, 708.0), Some("DP-2"));
        Ok(())
    }

    #[test]
    fn malformed_replies_are_errors() {
        assert!(matches!(
            serde_json::from_str::<Vec<HyprMonitor>>("ok"),
            Err(err) if err.is_syntax()
        ));
        assert!(matches!(
            serde_json::from_str::<Vec<HyprMonitor>>(r#"[{"name":"x"}]"#),
            Err(err) if err.is_data()
        ));
        assert!(matches!(
            serde_json::from_str::<HyprCursor>("{}"),
            Err(err) if err.is_data()
        ));
        let secret = r#"[{"name":"private-name"}]"#;
        if let Err(err) = serde_json::from_str::<Vec<HyprMonitor>>(secret) {
            let err = HyprlandError::from(err);
            assert!(matches!(err, HyprlandError::Json { line: 1, .. }));
            assert!(!format!("{err} {err:?}").contains("private-name"));
        }
    }

    #[test]
    fn socket_path_follows_the_environment() {
        let socket = hyprland_socket(Some("/run/user/1000".into()), Some("abc_1".into()));
        assert!(
            matches!(socket, Ok(path) if path == std::path::Path::new("/run/user/1000/hypr/abc_1/.socket.sock"))
        );
        for (runtime, signature) in [
            (None, Some("abc")),
            (Some("/run/user/1000"), None),
            (Some(""), Some("abc")),
            (Some("/run/user/1000"), Some("")),
        ] {
            assert!(matches!(
                hyprland_socket(runtime.map(OsString::from), signature.map(OsString::from)),
                Err(HyprlandError::NotHyprland)
            ));
        }
        for runtime in ["run/user/1000", "./run"] {
            assert!(matches!(
                hyprland_socket(Some(runtime.into()), Some("abc".into())),
                Err(HyprlandError::RuntimeDir)
            ));
        }
        for signature in ["..", "a/b", "/abs", "."] {
            assert!(matches!(
                hyprland_socket(Some("/run/user/1000".into()), Some(signature.into())),
                Err(HyprlandError::Signature)
            ));
        }
    }

    #[test]
    fn names_select_existing_outputs() {
        let wanted = ["DP-1".to_owned(), "HDMI-A-1".to_owned(), "DP-9".to_owned()];
        let available = [Some("HDMI-A-1"), None, Some("DP-1"), Some("DP-2")];
        assert_eq!(matching(&wanted, &available), [0, 2]);
        assert_eq!(
            matching(&["DP-9".to_owned()], &available),
            Vec::<usize>::new()
        );
        assert_eq!(matching(&wanted, &[]), Vec::<usize>::new());
    }

    #[test]
    fn stacks_have_spacing_between_popups() {
        assert_eq!(stack(&[], 8), (vec![], 0));
        assert_eq!(stack(&[100], 8), (vec![0], 100));
        assert_eq!(stack(&[100, 60, 80], 8), (vec![0, 108, 176], 256));
        assert_eq!(stack(&[i32::MAX, 10], 8), (vec![0, i32::MAX], i32::MAX));
    }
}
