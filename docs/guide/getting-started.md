# Getting started

::: warning Early development
touchcue works on Linux only and has no release yet. See [Platforms](./platforms) for what each platform supports.
:::

## What it does

When a hardware authenticator waits for a physical touch, touchcue shows a popup that takes no input focus, or a desktop notification. The prompt names the requesting application and the device. It detects:

- FIDO2/U2F keys of any vendor, read from their hidraw devices;
- OpenPGP cards used through gpg-agent, by `gpg` and by ssh through gpg-agent's ssh support. This needs [`touchcue gpg install`](./gpg).

## Build from source

Install the Rust toolchain pinned in `rust-toolchain.toml`, then run from the repository:

```sh
cargo install --locked --path crates/touchcue
```

This installs `touchcue` into `~/.cargo/bin`. Contributors can use `mise run build` instead, which builds every workspace crate into `target/`.

## Check the setup

```sh
touchcue check
```

It prints whether the configuration is valid, whether the desktop offers Wayland layer-shell, X11 and notifications, every FIDO device and whether touchcue can read it, the output backend `touchcue run` would choose, the IPC endpoints, and the gpg setup. It exits with status 1 when the configuration is invalid or a FIDO device cannot be read.

`touchcue list-devices` lists the connected FIDO devices. `touchcue trace` prints the detection signals until interrupted, which helps when a touch is not detected.

## Run the daemon

In the foreground:

```sh
touchcue run -v
```

`-v` logs at info level and `-vv` at debug level; the `TOUCHCUE_LOG` environment variable takes a `tracing` filter and overrides both.

### As a systemd user service

The repository has a user unit at `packaging/systemd/touchcue.service`. Nothing installs it, so copy it yourself:

```sh
mkdir -p ~/.config/systemd/user
cp packaging/systemd/touchcue.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now touchcue.service
```

The unit runs `touchcue run`, which systemd finds only in `/usr/local/bin` and `/usr/bin`. For a binary in `~/.cargo/bin`, override the command with `systemctl --user edit touchcue.service`:

```ini
[Service]
ExecStart=
ExecStart=%h/.cargo/bin/touchcue run
```

The unit starts with `graphical-session.target`, so it needs a session manager that activates that target.

## gpg and ssh through gpg-agent

```sh
touchcue gpg install
```

This makes gpg-agent start touchcue as its `scdaemon-program`. See [gpg and ssh](./gpg). For `ed25519-sk` and `ecdsa-sk` keys, the FIDO source shows the prompt already; [ssh askpass](./ssh-askpass) adds the key and remote user to it.

## Migrating from yubikey-touch-detector

1. Stop and disable its socket and service: `systemctl --user disable --now yubikey-touch-detector.socket yubikey-touch-detector.service`. A socket unit left active keeps the socket path, and touchcue skips its compatible socket.
2. Start touchcue as above.
3. If a status bar or script reads `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, turn on the compatible socket:

   ```toml
   [compat.maxbaz_socket]
   enabled = true
   ```

touchcue serves that socket at the same path with the same 5-byte messages, `U2F_1`/`U2F_0`, `GPG_1`/`GPG_0` and `MAC_1`/`MAC_0`. It skips the socket with a warning while yubikey-touch-detector still serves it. See [IPC](./configuration#ipc).

## Shell completions

The repository's `completions/` directory holds scripts for bash, zsh, fish, nushell and PowerShell. They need the [`usage`](https://usage.jdx.dev/cli/completions) CLI on `PATH`, and bash also needs bash-completion loaded first.

## Next steps

- [Configuration](./configuration)
- [Placeholders](./placeholders)
- [CLI reference](/reference/cli/)
