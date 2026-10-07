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
position = "center"
output = "focused"
min_display_ms = 800
show_delay_ms = 0
modal = false
modal_dim = 0.4
modal_dismiss = true

[notification]
urgency = "critical"
safety_timeout_s = 60

[templates]
title = 'Touch {device.vendor|"your security key"}'
body = '{requester.label|process.name|"An application"} is waiting for {request.method}'

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

[requester]
skip = [
  "sh", "bash", "dash", "zsh", "fish", "nu", "ksh", "mksh", "tcsh", "csh", "elvish", "xonsh",
  "tmux*", "screen", "zellij", "herdr", "abduco", "dtach",
  "timeout", "nice", "nohup", "setsid", "stdbuf", "time", "xargs", "flock", "ionice", "chrt",
  "taskset", "env", "sudo", "doas", "su", "run0",
  "systemd", "init",
]
extend_skip = []
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
| `position` | `center`, `top-left`, `top-right`, `bottom-left`, `bottom-right`, `top`, `bottom` | `center` |
| `output` | `"focused"`, `"all"`, `"cursor"`, or a list of output names | `"focused"` |
| `min_display_ms` | 0 to 600000 | 800 |
| `show_delay_ms` | 0 to 600000 | 0 |
| `modal` | `true`, `false` | `false` |
| `modal_dim` | 0.0 to 1.0 | 0.4 |
| `modal_dismiss` | `true`, `false` | `true` |

`min_display_ms` is the shortest time a shown popup stays visible. `show_delay_ms` is how long a request must wait before its popup appears.

Several popups on one output stack in the order the requests started: away from the screen edge, or vertically around the centre with `center`.

### Outputs

`output` chooses the monitors that show the popup. The popup's output is chosen when it appears.

- `"focused"` lets the Wayland compositor pick, usually the focused monitor. On X11 it is the monitor under the mouse pointer.
- `"all"` shows a popup on every output.
- `"cursor"` uses the output under the mouse pointer. On Wayland this works only on Hyprland, through its IPC socket. Elsewhere, or when the query fails, touchcue logs a warning and falls back to `"focused"`. On X11 it is the same as `"focused"`.
- A list such as `["DP-1", "HDMI-A-1"]` shows a popup on each listed output that exists. Wayland names come from the compositor (`hyprctl monitors`, `swaymsg -t get_outputs`), X11 names from RandR (`xrandr --listmonitors`). When none of them exists, touchcue logs a warning and falls back to `"focused"`. An empty list or an empty name is an error.

### Modal popups

With `modal = true`, a popup that waits for a touch also blocks clicks: on each of its outputs, a full-screen layer behind the popup takes every click. The keyboard is never grabbed, so typing still goes to the focused window. The overlay is removed when no request on that output waits any more, and after 120 seconds at most, even when requests still wait; requests that start while it is shown do not extend it. After those 120 seconds, that output gets no new overlay for 30 seconds.

- `modal_dim` is how dark the overlay is, up to `1.0` (black). `0.0` blocks clicks without dimming.
- With `modal_dismiss = true`, a click or touch on the overlay hides the popups and overlays of the requests on it. Only the display is hidden: the request keeps waiting for a touch. The next request on that output gets a new overlay right away. Only the left, middle and right buttons dismiss; scrolling and side buttons do not.
- With `modal_dismiss = false`, requests that follow each other can keep an output dimmed for longer than 120 seconds: the limit applies to each overlay, and a new request gets a new overlay once the previous one ended, or 30 seconds after it reached the limit. The keyboard keeps working throughout.

On X11 the overlay is an input-only window that blocks clicks without dimming, so `modal_dim` has no effect there. On Wayland the overlay is on the layer-shell overlay layer, above windows and panels.

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
| `body` | `{requester.label\|process.name\|"An application"} is waiting for {request.method}` |

The default body names the requester and its application through [`requester.label`](./placeholders#requester): `claude in Kitty is waiting for openpgp`, or `Firefox is waiting for fido2` when the application asks itself.

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

## `[[hooks]]`

Hooks run a command on touchcue events. See [Hooks](./hooks) for the events, the environment the command gets, and recipes. With hooks, this file runs commands as your user: keep it writable only by you, and never run the daemon as root with a configuration file another user can write.

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "fido2" }
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
timeout_ms = 5000
concurrency = 4
```

| Key | Type | Meaning |
| --- | --- | --- |
| `on` | list of event names | Events that run the command. Required and non-empty. An unknown name is an error. |
| `match` | table of strings | Placeholder names and values that must all be equal, as in `[[rules]]`. Optional; without it, every listed event runs the command. Device events only have `device.*` values and daemon events none, so a `match` on other names never runs for them. `match` only selects events: application and process names are set by the processes themselves. |
| `command` | list of strings | The program and its arguments, run without a shell. Required, and the program must not be empty. |
| `timeout_ms` | 1 to 600000 | Time the command may run before it is ended. Default 5000. |
| `concurrency` | 1 to 32 | Most runs of this hook at once. Default 4. |

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

## `[requester]`

| Key | Values | Default |
| --- | --- | --- |
| `skip` | list of process names | shells, multiplexers, wrappers and service managers, as in the example above |
| `extend_skip` | list of process names | `[]` |

These lists name the processes passed over when looking for the [requester](./placeholders#requester). `skip` replaces the built-in list, and `skip = []` passes over nothing; `extend_skip` adds names to `skip` or to the built-in list. To also pass over the editor whose terminal you work in:

```toml
[requester]
extend_skip = ["nvim"]
```

An entry is a name, compared exactly and case-sensitively with the process's `comm`, or with its executable's file name when `comm` cannot be read, or a prefix ending in `*`, such as `tmux*`. The kernel cuts `comm` to 15 bytes, so match longer names with a prefix. An invalid entry is reported with its location in the file. `touchcue check` prints the effective list; copy the list after `skip =` into the configuration to start from it.

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

`values` holds only these placeholders: every `request.*` key, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` and `requester.label`. Executable paths, pids, uids and command lines are never published. `process.chain` and `requester.*` do expose the names of the client's parent processes, such as the shell and tools that ran it, to every client of the socket and the bus name.

A second `touchcue run` exits while the event socket or the bus name is in use.
