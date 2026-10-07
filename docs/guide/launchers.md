# Launchers

touchcue can show the prompt through programs of your choice, such as rofi, fuzzel, `hyprctl notify` or a status bar script. touchcue contains no code for any of them: a [hook with `until = "ended"`](./hooks#hooks-that-last-for-a-request) runs the program for as long as a request lasts and gives it the prompt in `TOUCHCUE_TITLE` and `TOUCHCUE_BODY`, the text the popup shows, including `(cancelled)` or `(timed out)` while the prompt lingers, and on stdin a JSON line when the prompt is shown and when it goes away, and with `on_change = "stream"` one for every change in between. It runs on Linux.

```toml
[output]
mode = "none"

[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

`mode = "none"` turns the popup off, so the launcher replaces it; leave `[output]` out to show both.

## Examples

Each example has a configuration snippet and notes:

- [rofi](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/rofi): a message dialog per request.
- [fuzzel](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/fuzzel): an empty menu with the prompt.
- [hyprctl](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/hyprctl): a Hyprland notification.
- [waybar](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/waybar): a status bar module that counts the waiting requests.

## Choosing `on_change`

- A program that shows fixed text, such as rofi or fuzzel, uses the default `restart`: when a value of the request changes, touchcue stops it and starts it again with the new text.
- A program that reads stdin, such as the waybar script, uses `stream` and gets each change as an `update` line.
- A program that should only appear once per request uses `ignore`.

A program that exits before its request ends, such as a menu closed with Escape or `hyprctl notify`, which exits at once, is dismissed: touchcue does not start it again until a value of the request changes.

## Untrusted text

Application and process names, and so the title and body, come from the requesting processes.

- `command` runs exactly as written; touchcue never puts values into it. Run a shell only explicitly, quote the variables, and never paste a variable into the script text.
- A value can start with `-`. Pass it after `--`, attach it to its option as in `--prompt-only="$TOUCHCUE_BODY"`, or put fixed text in front of it, as the hyprctl example does.
- Programs that read Pango markup or HTML need the text escaped; the waybar example escapes it, and the rofi example does not turn markup on.
