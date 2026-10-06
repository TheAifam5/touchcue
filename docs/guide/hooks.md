# Hooks

A hook runs a command of your choice when a touchcue event happens, for example to play a sound when a request starts. Hooks are `[[hooks]]` entries in the [configuration](./configuration#hooks). They run on Linux.

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

`touchcue check` prints the number of configured hooks.

Hooks make the configuration file run commands as your user. Keep it writable only by you, and never run the daemon as root with a configuration file that another user can write.

## Events

| Event | When |
| --- | --- |
| `started` | A request started. |
| `updated` | A request changed, for example its state, client processes or detail. |
| `ended` | A request ended. |
| `waiting` | A request started, or was revived, and waits for a touch. |
| `lingering` | A request's operation ended without a touch. The request stays for `retry_window_ms` in case the client retries it. |
| `revived` | A client retried a lingering request, which waits again. |
| `touched` | The key was touched. |
| `cancelled` | The client cancelled the operation. |
| `failed` | The operation failed. |
| `timed_out` | The key stopped reporting the request, or the operation timed out. |
| `device_added` | touchcue started watching a FIDO key: when it is plugged in, and for every key present when the daemon starts. |
| `device_removed` | touchcue stopped watching a FIDO key, usually because it was unplugged. |
| `daemon_started` | The daemon started. |
| `daemon_stopping` | The daemon is stopping. |

One change can fire several events. They fire in this order: `started`, `updated` or `ended`, then `revived`, `waiting` or `lingering`, then `touched`, `cancelled`, `failed` or `timed_out`. For example, a new request fires `started`, then `waiting`.

`touched` fires when the request ends. `cancelled`, `failed` and `timed_out` fire as soon as the operation ends, together with `lingering`, and are not repeated by `ended`. A request that is revived after `cancelled` waits again, so it can end with `touched` later.

The device events only come from FIDO keys, while `[sources.fido]` is enabled. The `device_added` events of keys present at startup arrive after `daemon_started`. In the rare case that the kernel drops device notifications, touchcue fires `device_removed` and `device_added` again for every key.

`match` selects which events run a hook; it does not prove anything. Application and process names are set by the processes themselves, so any program can claim to be `ssh` or `firefox`.

## Environment

A command gets touchcue's own environment, so `PATH`, `WAYLAND_DISPLAY` and `DBUS_SESSION_BUS_ADDRESS` reach it, plus these variables:

- `TOUCHCUE_EVENT` is the event name, such as `started`.
- Each [placeholder](./placeholders) that touchcue publishes becomes a variable named `TOUCHCUE_` followed by the placeholder name in upper case with dots replaced by underscores. For example, `app.name` is `TOUCHCUE_APP_NAME` and `request.state` is `TOUCHCUE_REQUEST_STATE`.

The published placeholders are the same as on the [event socket](./configuration#ipc): every `request.*` value, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container` and `process.name`. Executable paths, pids, uids and command lines are never passed.

A value that is not known is not set. Control and invisible characters in a value are replaced by spaces, runs of whitespace become one space, and a value is cut after 1024 characters. A value can start with `-`, so pass `--` before it to programs that parse options. Device events set only the `device.*` variables, and `daemon_started` and `daemon_stopping` set only `TOUCHCUE_EVENT`.

Variables starting with `TOUCHCUE_` that touchcue itself inherited, such as `TOUCHCUE_LOG` or `TOUCHCUE_ASKPASS_FALLBACK` from your shell, are not passed to hooks: every `TOUCHCUE_` variable a hook sees comes from touchcue.

## Running

- **No shell.** `command` is the program and its arguments, run directly. `$HOME`, `*`, `;` or `|` in it are passed on as written. The program is looked up in `PATH`.
- **Values only in variables.** Placeholder values never become arguments, so a crafted application name cannot inject arguments. When you need a shell, run one explicitly and quote the variables, as in the `notify-send` recipe below. Never paste a variable into the script text.
- **Timeout.** A command that runs longer than `timeout_ms`, 5 seconds by default, gets `SIGTERM` sent to its own process group. `SIGKILL` follows to the group as soon as the command exits, or one second later if it does not. touchcue always waits for the command to exit.
- **Process cleanup.** Only processes still in the hook's process group are ended on a timeout or at shutdown. A process that leaves the group, for example with `setsid`, keeps running, and so do processes a command started in the background before it exited normally.
- **Input and output.** stdin is empty. The first 4 KiB of stdout and stderr are logged at debug level (`touchcue run -vv`), with control characters removed. After the command exits, touchcue reads its output for at most 500 ms more, so a command that leaves work running in the background should redirect that work's output, or the output is discarded.
- **Failures.** A command that cannot start, exits with a non-zero status, times out or is ended at shutdown is logged as a warning with the hook's index in the configuration and the event. Each hook warns at most once every 10 seconds and counts the warnings it held back; the others are logged at debug level.
- **Order and limits.** The runs of one hook start in the order of their events, at most `concurrency` at once, 4 by default. Runs that start in order can still finish out of order, so use `concurrency = 1` for effects that must happen one after another. Different hooks run independently. Each hook queues up to 256 events; when its queue is full, the newest event is dropped for that hook, the queued ones are kept, and touchcue logs a rate-limited warning.
- **Shutdown.** When touchcue stops, it fires `daemon_stopping`, then waits up to 2 seconds for the queued and running commands, whatever their `timeout_ms`, including the `daemon_stopping` commands. Commands still running then get `SIGTERM`, and `SIGKILL` as described for timeouts; queued runs that did not start are dropped.

touchcue never blocks on a hook: popups, notifications and IPC continue while commands run.

## Recipes

### Play a sound when a request starts

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

Without PipeWire, use `paplay` with the same file. Add `match` to play the sound only for some requests:

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "openpgp" }
command = ["paplay", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

### Notify when the key was touched

`notify-send` takes the text as arguments, so a shell reads it from the environment:

```toml
[[hooks]]
on = ["touched"]
command = ["sh", "-c", 'notify-send -- "Security key touched" "${TOUCHCUE_APP_NAME:-An application}"']
```

The single quotes make the TOML string literal, so the script reaches `sh` unchanged. The double quotes keep the application name a single argument, and `--` stops `notify-send` from reading a name that starts with `-` as an option.

### Refresh a waybar module

A waybar `custom` module with `"signal": 8` runs its `exec` again when waybar receives `SIGRTMIN+8`:

```toml
[[hooks]]
on = ["started", "ended"]
command = ["pkill", "-RTMIN+8", "waybar"]
concurrency = 1
```

`concurrency = 1` sends the signals one after another, in the order of the events, so the module's last refresh follows the last change.

### Show a Hyprland notification

```toml
[[hooks]]
on = ["waiting"]
command = ["hyprctl", "notify", "1", "5000", "rgb(ff1ea3)", "Touch your security key"]
```

The arguments are the icon (`1` is info), the time in milliseconds, the colour and the message.
