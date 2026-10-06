//! Parsing of the systemd unit names desktop environments assign to applications.

use touchcue_core::text::sanitize;
use tracing::trace;

use crate::TEXT_MAX;

/// Application identity carried by a systemd unit name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitApp {
    /// Launcher that started the application, such as `flatpak` or `gnome`.
    pub launcher: Option<String>,
    /// Application id, normally the desktop file id without `.desktop`.
    pub app_id: String,
}

/// Parses `app[-<launcher>]-<ApplicationID>[@<RANDOM>].service` or
/// `app[-<launcher>]-<ApplicationID>-<RANDOM>.scope`.
///
/// Fields are split before `\xNN` escapes are decoded, so an escaped dash
/// stays inside its field; a malformed escape is kept literally. Decoded
/// fields are passed through [`sanitize`] with a 128-char cap; a launcher left empty becomes `None`. Returns `None`
/// for any other name, including a scope without a `<RANDOM>` part or an
/// application id left empty.
#[must_use]
pub fn parse(unit: &str) -> Option<UnitApp> {
    let rest = unit.strip_prefix("app-")?;
    let body = if let Some(service) = rest.strip_suffix(".service") {
        service.rsplit_once('@').map_or(service, |(body, _)| body)
    } else {
        rest.strip_suffix(".scope")?.rsplit_once('-')?.0
    };
    let mut parts = body.split('-');
    let first = parts.next()?;
    let (launcher, app_id) = match (parts.next(), parts.next()) {
        (None, _) => (None, first),
        (Some(app_id), None) => (Some(first), app_id),
        (Some(_), Some(_)) => return None,
    };
    if app_id.is_empty() || launcher.is_some_and(str::is_empty) {
        return None;
    }
    Some(UnitApp {
        launcher: launcher.and_then(|launcher| sanitize(&unescape(launcher), TEXT_MAX)),
        app_id: sanitize(&unescape(app_id), TEXT_MAX)?,
    })
}

fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(value) = bytes.get(i + 2..i + 4).and_then(hex_pair)
        {
            out.push(value);
            i += 4;
            continue;
        }
        out.push(byte);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
    if !pair.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let digits = match std::str::from_utf8(pair) {
        Ok(digits) => digits,
        Err(error) => {
            trace!(
                error = &error as &dyn std::error::Error,
                "unit name escape is not UTF-8"
            );
            return None;
        }
    };
    match u8::from_str_radix(digits, 16) {
        Ok(byte) => Some(byte),
        Err(error) => {
            trace!(
                error = &error as &dyn std::error::Error,
                "unit name escape unparsable"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(launcher: Option<&str>, app_id: &str) -> UnitApp {
        UnitApp {
            launcher: launcher.map(str::to_owned),
            app_id: app_id.to_owned(),
        }
    }

    #[test]
    fn parses_observed_names() {
        let cases = [
            (
                "app-flatpak-com.discordapp.Discord-1323191083.scope",
                app(Some("flatpak"), "com.discordapp.Discord"),
            ),
            (
                "app-org.chromium.Chromium-10581.scope",
                app(None, "org.chromium.Chromium"),
            ),
            (
                r"app-easyeffects\x2dservice@autostart.service",
                app(None, "easyeffects-service"),
            ),
            (
                r"app-polkit\x2dgnome\x2dauthentication\x2dagent\x2d1@autostart.service",
                app(None, "polkit-gnome-authentication-agent-1"),
            ),
            (
                "app-gnome-org.gnome.Evince@12345.service",
                app(Some("gnome"), "org.gnome.Evince"),
            ),
            ("app-org.kde.amarok.service", app(None, "org.kde.amarok")),
            (r"app-foo\x2@x.service", app(None, r"foo\x2")),
            (r"app-foo\xzz.service", app(None, r"foo\xzz")),
            (r"app-\xc3\xa9t\xc3\xa9.service", app(None, "été")),
            (
                r"app-Evil\x1b[2J\x0aText-1.scope",
                app(None, "Evil [2J Text"),
            ),
            (r"app-\x1b-x-1.scope", app(None, "x")),
            (r"app-x\x2fy-1.scope", app(None, "x/y")),
        ];
        for (unit, expected) in cases {
            assert_eq!(parse(unit), Some(expected), "{unit}");
        }
    }

    #[test]
    fn rejects_other_names() {
        for unit in [
            "app-graphical.slice",
            "kitty-6397-0.scope",
            "app-kitty.scope",
            "app-a-b-c.service",
            "app-a-b-c-1.scope",
            "app-.service",
            "app-@x.service",
            "app--foo.service",
            "session-2.scope",
            "app-",
            r"app-\x0a\x09-1.scope",
            "",
        ] {
            assert_eq!(parse(unit), None, "{unit}");
        }
    }
}
