# fuzzel

Shows each touch prompt as the prompt of an empty [fuzzel](https://codeberg.org/dnkl/fuzzel) menu, instead of the popup. Add [`config.toml`](config.toml) to `~/.config/touchcue/config.toml`; remove its `[output]` section to keep the popup as well.

Not tested by the maintainer. The flags are taken from fuzzel's manual page, `fuzzel(1)`.

- `--dmenu --prompt-only=TEXT` shows `TEXT` as the prompt, reads nothing from stdin and lists no entries, so touchcue's JSON lines on stdin are ignored. Attaching the body to the option with `=` keeps a body that starts with `-` from being read as an option.
- `--keyboard-focus=on-demand` lets other windows keep the keyboard; the default, `exclusive`, takes all keyboard input until fuzzel closes.
- fuzzel exits when it loses keyboard focus. touchcue treats any exit before the request ends as a dismissal and does not start fuzzel again until the request's values change. Add `--no-exit-on-keyboard-focus-loss` to keep it open.
- touchcue stops fuzzel with `SIGTERM` when the request ends, and starts a new one when the request's values change.
