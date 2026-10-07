# Hooks

A hook runs a command of your choice when a touchcue event happens, for example to play a sound when a request starts. Hooks are `[[hooks]]` entries in the [configuration](./configuration#hooks). They run on Linux.

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

`touchcue check` prints the number of configured hooks, and how many of them have `until`.

A hook with `until = "ended"` runs one process for each request, for as long as the request lasts, instead of once per event. That is how a launcher such as rofi or fuzzel shows the prompt; see [Hooks that last for a request](#hooks-that-last-for-a-request) and [Launchers](./launchers).

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

The published placeholders are the same as on the [event socket](./configuration#ipc): every `request.*` value, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` and `requester.label`. Executable paths, pids, uids and command lines are never passed.

A value that is not known is not set. Control and invisible characters in a value are replaced by spaces, runs of whitespace become one space, and a value is cut after 1024 characters. A value can start with `-`, so pass `--` before it to programs that parse options. Device events set only the `device.*` variables, and `daemon_started` and `daemon_stopping` set only `TOUCHCUE_EVENT`.

Variables starting with `TOUCHCUE_` that touchcue itself inherited, such as `TOUCHCUE_LOG` or `TOUCHCUE_ASKPASS_FALLBACK` from your shell, are not passed to hooks: every `TOUCHCUE_` variable a hook sees comes from touchcue.

## Running

- **No shell.** `command` is the program and its arguments, run directly. `$HOME`, `*`, `;` or `|` in it are passed on as written. The program is looked up in `PATH`.
- **Values only in variables.** Placeholder values never become arguments, so a crafted application name cannot inject arguments. When you need a shell, run one explicitly and quote the variables, as in the `notify-send` recipe below. Never paste a variable into the script text.
- **Timeout.** A command that runs longer than `timeout_ms`, 5 seconds by default, gets `SIGTERM` sent to its own process group. `SIGKILL` follows to the group as soon as the command exits, or one second later if it does not. touchcue always waits for the command to exit.
- **Process cleanup.** Only processes still in the hook's process group are ended on a timeout or at shutdown. A process that leaves the group, for example with `setsid`, keeps running, and so do processes a command started in the background before it exited normally.
- **Input and output.** stdin is empty, except for [hooks with `until`](#standard-input). The first 4 KiB of stdout and stderr are logged at debug level (`touchcue run -vv`), with control characters removed; the output of hooks with `until` is never logged, since a launcher can print what you type into it. After the command exits, touchcue reads its output for at most 500 ms more, so a command that leaves work running in the background should redirect that work's output, or the output is discarded.
- **Failures.** A command that cannot start, exits with a non-zero status, times out or is ended at shutdown is logged as a warning with the hook's index in the configuration and the event. Each hook warns at most once every 10 seconds and counts the warnings it held back; the others are logged at debug level.
- **Order and limits.** The runs of one hook start in the order of their events, at most `concurrency` at once, 4 by default. Runs that start in order can still finish out of order, so use `concurrency = 1` for effects that must happen one after another. Different hooks run independently. Each hook queues up to 256 events; when its queue is full, the newest event is dropped for that hook, the queued ones are kept, and touchcue logs a rate-limited warning.
- **Shutdown.** When touchcue stops, it fires `daemon_stopping`, then waits up to 2 seconds for the queued and running commands, whatever their `timeout_ms`, including the `daemon_stopping` commands. Commands still running then get `SIGTERM`, and `SIGKILL` as described for timeouts; queued runs that did not start are dropped.

touchcue never blocks on a hook: popups, notifications and IPC continue while commands run.

## Hooks that last for a request

With `until = "ended"`, a hook runs one process for each request, which lasts until the request ends. It suits programs that show the prompt themselves, such as a rofi dialog:

```toml
[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

| Key | Values | Default |
| --- | --- | --- |
| `until` | `ended` | none |
| `on_change` | `restart`, `stream`, `ignore` | `restart` |
| `stop_signal` | `SIGTERM`, `SIGINT`, `SIGHUP` | `SIGTERM` |
| `stop_grace_ms` | 0 to 2000 | 1000 |
| `restart_interval_ms` | 0 to 600000 | 200 |

`on`, `match` and `command` work as for other hooks, with these limits:

- `on` may list only `started` and `waiting`; another event is an error.
- `timeout_ms` and `concurrency` are errors: the process lasts as long as its request, and at most 8 processes of all hooks with `until` run at once. A process beyond that starts as soon as another one ended, with a rate-limited warning.
- `on_change`, `stop_signal`, `stop_grace_ms` and `restart_interval_ms` are errors without `until`.

### When the process runs

- It starts when one of the `on` events fires for a request whose values match `match`, as soon as the request's prompt is shown. Shown means that no rule suppresses it, whatever `[output]` displays, so a hook with `until` also works with `mode = "none"`.
- While a [rule](./configuration#rules) suppresses the prompt, the process is stopped, and a suppressed request does not start one. When the prompt is shown again, a new process starts.
- It is stopped when the request ends.
- `waiting` also fires when a client retry revives a lingering request, so a hook on `waiting` starts a process again that was dismissed.

`on_change` decides what happens when a value of the request changes:

- `restart` stops the process and starts a new one once the old one exited, at most once per `restart_interval_ms`; changes in between are coalesced into the latest values. A change of `request.elapsed` alone does not restart it.
- `stream` keeps the process and writes each change to its stdin, including a change of `request.elapsed` alone.
- `ignore` keeps the process and tells it nothing.

A process that exits before its request ends, with any exit status, or that cannot start, is dismissed, for example a menu closed with Escape. touchcue does not start it again until a value of the request changes, or with `on_change = "ignore"` until one of the `on` events fires again. A change that arrived before the exit counts: with `restart`, a process that exits while a restart waits for `restart_interval_ms` is started again with the newest values. When the process exits on its own, touchcue also sends `SIGKILL` to its process group, so programs it left running in the background end with it and do not escape the limit of 8 processes.

### Environment of a process

A process gets the [environment](#environment) of other hooks, with these differences:

- `TOUCHCUE_EVENT` is `show` for the first process of a request, and `update` for a process started again after a change or after the prompt was suppressed.
- `TOUCHCUE_TITLE` and `TOUCHCUE_BODY` are the prompt title and body exactly as the popup and the notification show them, after rules, with the same cleaning and length limit as the other values. While a request lingers after it was cancelled or timed out, the body ends with `(cancelled)` or `(timed out)`, as on the popup; `TOUCHCUE_REQUEST_STATE` holds the state for a script that words the outcome itself. They are rendered from your own templates, so they can hold placeholders that are not published, such as a command line, when your templates use them.

Other hooks get no `TOUCHCUE_TITLE` or `TOUCHCUE_BODY`: they also run for suppressed and ended requests, which have no prompt.

### Standard input

The stdin of a process is a pipe that carries one JSON object per line:

```json
{"op":"show","id":3,"title":"Touch Yubico","body":"Firefox is waiting for fido2","values":{"app.name":"Firefox","request.method":"fido2","request.state":"waiting"}}
{"op":"update","id":3,"title":"Touch Yubico","body":"Firefox is waiting for fido2 (cancelled)","values":{"app.name":"Firefox","request.method":"fido2","request.state":"cancelled"}}
{"op":"hide","id":3}
```

- `op` is `show` or `update`, as in `TOUCHCUE_EVENT`, for the first line, `update` for each later change with `on_change = "stream"`, and `hide` when the request ended or its prompt was suppressed.
- `id` is the request id, a number.
- `title` and `body` are the strings of `TOUCHCUE_TITLE` and `TOUCHCUE_BODY`, including the outcome while the request lingers; `request.state` in `values` tells the outcome on its own.
- `values` holds the published placeholders by name, cleaned as in the environment. The example shows only some of them.

touchcue never waits for a process to read. Each process has a queue of 64 lines; when it is full, the oldest queued `update` is dropped with a rate-limited warning, while the `show` and `hide` lines are always kept. A process that closes stdin or exits only loses its lines. A `stream` process that reads nothing while 256 updates are dropped is stopped, with a rate-limited warning. It is started again with the newest values, at most once per `restart_interval_ms`, when a value other than `request.elapsed` changed since the last update it was given; otherwise it is dismissed.

### Stopping a process

To stop a process, touchcue writes its remaining lines and the `hide` line, closes stdin, and then sends `stop_signal` to the process group. Writing takes at most 200 ms; a process that does not read in that time gets the signal anyway, and may see a last line cut short before the end of its input. The signal follows closing stdin at once, so a program that needs to act on the `hide` line or on the end of its input must handle that signal, for example by ignoring it. `SIGKILL` follows to the group `stop_grace_ms` later if the process has not exited, and touchcue always waits for it.

A process stopped for a restart gets no `hide` line.

When touchcue stops, it writes no more lines; the processes get `stop_signal` at once, and `SIGKILL` after `stop_grace_ms`. This fits within the 4 seconds that the daemon gives all hooks at shutdown; the systemd unit allows the whole daemon 10 seconds to stop.

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
