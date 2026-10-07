# hyprctl notify

Shows each touch prompt as a [Hyprland](https://hypr.land) notification, next to the popup. Add [`config.toml`](config.toml) to `~/.config/touchcue/config.toml`; add `[output]` with `mode = "none"` for the notification alone.

The arguments of `hyprctl notify` are the icon (`1` is info), the time in milliseconds, the colour and the message. `hyprctl --help` documents no `--` to end its options, so the message starts with the fixed text `Touch: `: a body that starts with `-` could otherwise be read as an option.

`hyprctl notify` sends the notification and exits at once. touchcue treats that exit as a dismissal, so it runs `hyprctl` again only when the request's values change, at most once a second with `restart_interval_ms = 1000`.

`hyprctl` cannot replace or withdraw one notification: `hyprctl dismissnotify` dismisses all of them or a number of them, never a chosen one. A notification therefore stays for its full time after the request ended, and a changed prompt adds a second notification instead of updating the first.
