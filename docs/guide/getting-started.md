# Getting started

::: warning Early development
touchcue works on Linux only. Release archives for Windows and macOS exist but detect nothing yet; see [Platforms](./platforms).
:::

## What it does

When a hardware authenticator waits for a physical touch, touchcue shows a popup that takes no input focus, or a desktop notification. The prompt names the program that asked for the touch, such as the browser or the tool that ran `git`, and the device. It detects:

- FIDO2/U2F keys of any vendor, read from their hidraw devices;
- OpenPGP cards used through gpg-agent, by `gpg` and by ssh through gpg-agent's ssh support. This needs [`touchcue gpg install`](./gpg).

## Install

### From a release archive

Each [release](https://github.com/theaifam5/touchcue/releases) has a `touchcue-<target>.tar.gz` archive per platform and a `SHA256SUMS` file. For x86_64 Linux with glibc:

```sh
curl -LO https://github.com/theaifam5/touchcue/releases/download/v0.1.1/touchcue-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/theaifam5/touchcue/releases/download/v0.1.1/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf touchcue-x86_64-unknown-linux-gnu.tar.gz
cd touchcue-x86_64-unknown-linux-gnu
```

Linux archives exist for `x86_64` and `aarch64`, each for `gnu` and `musl`. An archive holds `touchcue`, `touchcue-helper`, `completions/`, `man/touchcue.1`, the licences and the README. Install the binary, the man page and the completions for your shell:

```sh
install -D touchcue ~/.local/bin/touchcue
install -Dm644 man/touchcue.1 ~/.local/share/man/man1/touchcue.1
install -Dm644 completions/touchcue.bash ~/.local/share/bash-completion/completions/touchcue
install -Dm644 completions/touchcue.fish ~/.config/fish/completions/touchcue.fish
```

`completions/` also holds `touchcue.zsh`, `touchcue.nu` and `touchcue.ps1`. The completions need the [`usage`](https://usage.jdx.dev/cli/completions) CLI on `PATH`, and bash also needs bash-completion loaded first. `touchcue-helper` does nothing yet and need not be installed.

### With mise

```sh
mise use -g packslip:github.com/theaifam5/touchcue
```

A `minimum_release_age` setting in your mise configuration can hide a release younger than that age.

### From source

Install the Rust toolchain pinned in `rust-toolchain.toml`, then run from the repository:

```sh
cargo install --locked --path crates/touchcue
```

This installs `touchcue` into `~/.cargo/bin`. Contributors can use `mise run build` instead, which builds every workspace crate into `target/`. The repository's `completions/` and `man/` directories hold the same files as the archives.

## Check the setup

```sh
touchcue check
```

It prints whether the configuration is valid, whether a Wayland compositor, Wayland layer-shell and a notification service are available, every FIDO device and whether touchcue can read it, the output backend `touchcue run` would choose, the IPC endpoints, the gpg setup, the number of hooks with how many of them have `until`, the requester skip list, and the [icon theme](./configuration#icons) with where it came from. The requester lines show how touchcue reads the terminal it runs in, with skipped processes in brackets:

```text
requester: skip = ["sh", "bash", …, "systemd", "init"] (defaults)
requester: here: touchcue ← [nu] ← [herdr] ← [herdr] ← [nu] ← kitty → "touchcue in kitty"
icons: theme = "Papirus-Dark" (portal org.gnome.desktop.interface)
```

It exits with status 1 when the configuration is invalid or a FIDO device cannot be read.

`touchcue list-devices` lists the connected FIDO devices. `touchcue trace` prints the detection signals until interrupted, which helps when a touch is not detected.

## Run the daemon

In the foreground:

```sh
touchcue run -v
```

`-v` logs at info level and `-vv` at debug level; the `TOUCHCUE_LOG` environment variable takes a `tracing` filter and overrides both.

### As a systemd user service

The archives do not include the systemd unit. Download it from the repository, at the tag of your release:

```sh
mkdir -p ~/.config/systemd/user
curl -Lo ~/.config/systemd/user/touchcue.service \
  https://raw.githubusercontent.com/theaifam5/touchcue/v0.1.1/packaging/systemd/touchcue.service
```

The unit runs `touchcue run`, which systemd finds only in `/usr/local/bin` and `/usr/bin`. For any other install path, override the command with `systemctl --user edit touchcue.service`, for example for `~/.local/bin`:

```ini
[Service]
ExecStart=
ExecStart=%h/.local/bin/touchcue run
```

Use `%h/.cargo/bin/touchcue` after `cargo install`. For mise, `mise which touchcue` prints the path, which changes with every upgrade. Then start it:

```sh
systemctl --user daemon-reload
systemctl --user enable --now touchcue.service
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

## Next steps

- [Configuration](./configuration)
- [Placeholders](./placeholders)
- [CLI reference](/reference/cli/)
