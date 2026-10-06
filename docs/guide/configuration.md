# Configuration

touchcue reads `$XDG_CONFIG_HOME/touchcue/config.toml`, else `~/.config/touchcue/config.toml`. `--config <PATH>` names another file, which must then exist. A missing default file means the defaults below.

Every key is optional. Unknown keys and out-of-range values are errors: `touchcue run` refuses to start and `touchcue check` reports them, both with the location in the file where it is known.

## Example

The defaults, written out:

```toml
[output]
mode = "popup"
fallback = "notification"

[popup]
position = "top-right"
min_display_ms = 800
show_delay_ms = 0

[notification]
urgency = "critical"
safety_timeout_s = 60

[templates]
title = 'Touch {device.vendor|"your security key"}'
body = '{app.name|process.name|"An application"} is waiting for {request.method}'

[sources.fido]
enabled = true
keepalive_timeout_ms = 1500
retry_window_ms = 1000

[sources.gpg]
enabled = true

[ipc]
enabled = true

[dbus]
enabled = true

[compat.maxbaz_socket]
enabled = false
```

## `[output]`

| Key | Values | Default |
| --- | --- | --- |
| `mode` | `popup`, `notification`, `both`, `none`, `command` | `popup` |
| `fallback` | the same | `notification` |

`mode` is used when the desktop supports it, else `fallback`, else nothing is shown.

- `popup` shows an overlay popup that takes no input focus. It uses Wayland layer-shell when `WAYLAND_DISPLAY` or `WAYLAND_SOCKET` is set and the compositor supports it, else X11 when `DISPLAY` is set, which includes XWayland.
- `notification` sends a desktop notification over D-Bus.
- `both` shows the popup and a notification.
- `none` shows nothing.
- `command` is planned. It is accepted, but skipped with a warning.

## `[popup]`

| Key | Values | Default |
| --- | --- | --- |
| `position` | `top-left`, `top-right`, `bottom-left`, `bottom-right`, `top`, `bottom` | `top-right` |
| `min_display_ms` | 0 to 600000 | 800 |
| `show_delay_ms` | 0 to 600000 | 0 |

`min_display_ms` is the shortest time a shown popup stays visible. `show_delay_ms` is how long a request must wait before its popup appears.

Centring, choosing the monitor and a modal mode are planned.

## `[notification]`

| Key | Values | Default |
| --- | --- | --- |
| `urgency` | `low`, `normal`, `critical` | `critical` |
| `safety_timeout_s` | 0 to 600 | 60 |

`safety_timeout_s` is the time after which the notification server withdraws a notification even if the request still waits; touchcue passes it as the notification's expiry, where `0` means never.

## `[templates]`

| Key | Default |
| --- | --- |
| `title` | `Touch {device.vendor\|"your security key"}` |
| `body` | `{app.name\|process.name\|"An application"} is waiting for {request.method}` |

Templates use [placeholders](./placeholders). A request that ended without a touch keeps its prompt briefly, with `(cancelled)` or `(timed out)` appended to the body.

## `[[rules]]`

Rules change the prompt for matching requests. The first rule whose `match` entries all equal the request's placeholder values applies; later rules are ignored.

```toml
[[rules]]
match = { "process.name" = "ssh", "request.method" = "fido2" }
title = "Touch your key for ssh"
icon = "/usr/share/icons/hicolor/scalable/apps/utilities-terminal.svg"

[[rules]]
match = { "app.id" = "org.mozilla.firefox" }
suppress = true
```

| Key | Type | Meaning |
| --- | --- | --- |
| `match` | table of strings | Placeholder names and values. Required and non-empty. Values compare exactly and case-sensitively; a placeholder without a value never matches. |
| `title` | template | Replaces `templates.title`. |
| `body` | template | Replaces `templates.body`. |
| `icon` | path | PNG or SVG file shown instead of the application's icon. |
| `suppress` | boolean | Shows nothing for matching requests. IPC still publishes them. Default `false`. |

## `[sources.fido]`

| Key | Values | Default |
| --- | --- | --- |
| `enabled` | boolean | `true` |
| `keepalive_timeout_ms` | 100 to 600000 | 1500 |
| `retry_window_ms` | 1 to 600000 | 1000 |

`keepalive_timeout_ms` is how long a FIDO request survives without a keepalive from the key. `retry_window_ms` is how long an ended operation waits for the client to retry it, so a retry continues the same prompt.

## `[sources.gpg]`

| Key | Values | Default |
| --- | --- | --- |
| `enabled` | boolean | `true` |

Turns on the helper socket `$XDG_RUNTIME_DIR/touchcue/helper.sock`, through which [the gpg-agent wrapper](./gpg) and [`touchcue askpass`](./ssh-askpass) report operations.

## IPC

| Section | Key | Default |
| --- | --- | --- |
| `[ipc]` | `enabled` | `true` |
| `[dbus]` | `enabled` | `true` |
| `[compat.maxbaz_socket]` | `enabled` | `false` |

All endpoints need `XDG_RUNTIME_DIR`. They only send; clients cannot change anything.

- **Event socket**, `$XDG_RUNTIME_DIR/touchcue/events.sock`, from `[ipc]`. Each line is one JSON object with `kind` (`started`, `updated` or `ended`), `id`, `state`, `reason` (set only for `ended`: `touched`, `cancelled`, `failed` or `timed_out`), `source` and `values`. A new client first gets a `started` line for every active request. Only processes of the same user may connect.
- **D-Bus**, from `[dbus]`: the session bus name `io.github.theaifam5.Touchcue`, object `/io/github/theaifam5/Touchcue`, interface `io.github.theaifam5.Touchcue1`. It has the property `Active`, the method `ActiveRequests`, and the signals `RequestStarted`, `RequestUpdated` and `RequestEnded`.
- **Compatible socket**, `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, from `[compat.maxbaz_socket]`. It sends the 5-byte messages of yubikey-touch-detector: `U2F_1`/`U2F_0` for FIDO, `GPG_1`/`GPG_0` for gpg and ssh, and `MAC_1`/`MAC_0`. Only requests that still wait for a touch count.

`values` holds only these placeholders: every `request.*` key, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container` and `process.name`. Executable paths, pids, uids and command lines are never published.

A second `touchcue run` exits while the event socket or the bus name is in use.
