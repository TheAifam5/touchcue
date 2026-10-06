# OpenSSH security keys

For `ed25519-sk` and `ecdsa-sk` keys, the FIDO source already shows the touch popup. `touchcue askpass` adds which key is being used and, when ssh-agent signs, for which remote user. The text is available in templates as `{request.detail}`, for example `ED25519-SK SHA256:… → user git`. OpenSSH does not report the destination host.

`touchcue askpass` never creates a popup of its own: it only adds detail to the FIDO popup for the same touch.

## How it works

OpenSSH starts the `SSH_ASKPASS` program for a user-presence notice and sends it `SIGTERM` when the check ends, whether the key was touched or not. `touchcue askpass` reports the notice to the daemon and waits for that signal.

OpenSSH only starts askpass for a notice when `DISPLAY` or `WAYLAND_DISPLAY` is set, or `SSH_ASKPASS_REQUIRE=force`. A plain `ssh` whose standard error is a terminal prints the notice to the terminal instead and does not start askpass.

When ssh-agent holds the key, the agent sends the notice, so the variables below must be in the agent's environment. For a systemd user service, use `systemctl --user set-environment` or an `Environment=` drop-in.

## Setup

`SSH_ASKPASS` takes a program path without arguments. touchcue runs as askpass when it is started through a link named `touchcue-askpass`:

```sh
ln -s "$(command -v touchcue)" ~/.local/bin/touchcue-askpass
```

Then set, in the environment of ssh or ssh-agent:

```sh
export SSH_ASKPASS="$HOME/.local/bin/touchcue-askpass"
export SSH_ASKPASS_REQUIRE=prefer   # or "force" when DISPLAY/WAYLAND_DISPLAY is not set
export TOUCHCUE_ASKPASS_FALLBACK=/usr/lib/ssh/ssh-askpass
```

`touchcue askpass <message>` does the same and is useful for testing; every argument after `askpass` is passed through unparsed.

The daemon must have `[sources.gpg]` enabled, which is the default; askpass reports go through its helper socket.

## Passphrases

touchcue never reads passphrases. Every other askpass prompt, passphrases and `confirm` prompts, is passed unchanged to the program in `TOUCHCUE_ASKPASS_FALLBACK`, your usual askpass such as `ssh-askpass`, `ksshaskpass` or `lxqt-openssh-askpass`. If that variable is unset, `touchcue askpass` prints an error and exits 1, which OpenSSH treats as a cancelled prompt. The fallback must not point back to touchcue.

## Without the daemon

If the daemon is not running, `touchcue askpass` still waits for OpenSSH's signal and exits quietly, so ssh is never blocked. After 600 seconds without a signal it exits on its own.
