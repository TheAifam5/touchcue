# rofi

Shows each touch prompt in a [rofi](https://github.com/davatorium/rofi) message dialog (`rofi -e`), instead of the popup. Add [`config.toml`](config.toml) to `~/.config/touchcue/config.toml`; remove its `[output]` section to keep the popup as well.

The hook has `until = "ended"`, so touchcue starts one rofi per request and stops it with `SIGINT` when the request ends. When the request's values change, touchcue stops rofi and starts a new one. Closing the dialog yourself dismisses the prompt until the values change again.

- Do not add `-pid` or `-replace`: touchcue starts and stops each rofi itself.
- rofi grabs the keyboard on Wayland while the dialog is shown.
- The text is shown as plain text. Do not add `-markup`: the application name comes from the requesting process and could carry Pango markup. If your rofi configuration turns markup on for everything, escape `&`, `<` and `>` in the body first, or use the fuzzel example:

  ```toml
  command = ["sh", "-c", 'exec rofi -e "$(printf %s "$TOUCHCUE_BODY" | sed "s/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g")"']
  ```
- A body that is exactly `-` makes rofi read the message from stdin, where it finds touchcue's JSON lines. The default body template never renders `-` alone.
