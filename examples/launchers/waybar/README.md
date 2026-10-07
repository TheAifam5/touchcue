# waybar

Shows the number of requests waiting for a touch in a [waybar](https://github.com/Alexays/Waybar) custom module, with their prompts as the tooltip. touchcue runs one script per request, with `on_change = "stream"`, so the script reads every change of its request on stdin.

1. Copy [`touchcue-waybar`](touchcue-waybar) and [`touchcue-waybar-status`](touchcue-waybar-status) to `~/.local/bin/` and make them executable. They need [jq](https://jqlang.org).
2. Add [`config.toml`](config.toml) to `~/.config/touchcue/config.toml`, with the absolute path of `touchcue-waybar` in `command`.
3. Add the module in [`module.jsonc`](module.jsonc) to the waybar configuration, and `custom/touchcue` to one of its module lists.

`touchcue-waybar` keeps its request's body in `$XDG_RUNTIME_DIR/touchcue-waybar/<pid>.json` and sends waybar `SIGRTMIN+8` after each change, which makes the module, with `"signal": 8`, run `touchcue-waybar-status` again. When the request ends, touchcue writes a `hide` line and stops the script with `SIGTERM`; the script removes its file either way. `touchcue-waybar-status` also removes files whose process is gone, such as after a `SIGKILL`. The module has the class `waiting` while a request waits, else `idle`, for styling. The directory and files are readable only by you, and both scripts refuse a directory that another user owns.

See [Hooks that last for a request](https://touchcue.theaifam5.cc/guide/hooks#hooks-that-last-for-a-request) for the JSON lines on stdin.
