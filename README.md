# touchcue

Shows who is waiting for your security key touch.

[![CI](https://github.com/theaifam5/touchcue/actions/workflows/ci.yml/badge.svg)](https://github.com/theaifam5/touchcue/actions/workflows/ci.yml)

> **Status: early development, Linux only, not released yet.** FIDO2/U2F keys and OpenPGP cards through gpg-agent work on Linux. Windows and macOS build but detect nothing.

## What it does

touchcue is a daemon and CLI. When a hardware authenticator waits for a physical touch, it shows a popup that does not take focus, centred by default, on the monitors you choose, or a desktop notification. An optional modal mode dims the screen and blocks clicks until you touch the key or dismiss the prompt. The prompt names the requesting application and the device, using templates you define with placeholders. Other programs, such as status bars, can follow requests over a JSON socket, D-Bus, or the socket protocol of yubikey-touch-detector.

## touchcue and yubikey-touch-detector

[yubikey-touch-detector](https://github.com/maximbaz/yubikey-touch-detector) by Maxim Baz is the established tool for this job and the inspiration for touchcue. touchcue takes a different approach in some areas and speaks the same socket protocol, so status bars and scripts written for yubikey-touch-detector keep working.

| | touchcue | yubikey-touch-detector |
| --- | --- | --- |
| Platforms | Linux; Windows and macOS planned | Linux and macOS |
| FIDO2/U2F | yes | yes |
| OpenPGP through gpg-agent (gpg, ssh) | yes | yes |
| HMAC/OTP | planned | yes |
| Shows the requesting application | yes, name and icon | no |
| Output | own popup on Wayland and X11 (placement, monitor choice, optional modal dim), or desktop notifications | desktop notifications |
| Message templates | placeholders for app, process, device and request | `{{.Reasons}}` |
| Integration | JSON socket, D-Bus, and the yubikey-touch-detector socket | yubikey-touch-detector socket, stdout |
| Packages | none yet | Arch Linux, Nix, Homebrew, GitHub releases, `go install` |

To switch, see [Migrating from yubikey-touch-detector](https://touchcue.theaifam5.cc/guide/getting-started#migrating-from-yubikey-touch-detector).

## Supported authenticators

| Authenticator | Linux | Windows | macOS |
| --- | --- | --- | --- |
| FIDO2/U2F keys of any vendor, including `ed25519-sk` and `ecdsa-sk` ssh keys | yes | planned | planned |
| OpenPGP cards through gpg-agent, for gpg and for ssh through gpg-agent | yes | planned | planned |
| PIV | planned | planned | planned |
| YubiKey HMAC/OTP | planned | planned | planned |
| OATH with touch | planned | planned | planned |
| fprintd fingerprint readers | planned | n/a | n/a |
| Trezor and Ledger hardware wallets | planned | planned | planned |

## Install

There is no release yet. Build from source with the Rust toolchain pinned in `rust-toolchain.toml`:

```sh
cargo install --locked --path crates/touchcue
```

## Quick start

```sh
touchcue check        # configuration, desktop, devices, IPC and gpg setup
touchcue run -v       # run the daemon in the foreground
touchcue gpg install  # optional: detect gpg and ssh through gpg-agent
```

To run touchcue as a systemd user service, see [Getting started](https://touchcue.theaifam5.cc/guide/getting-started).

## Documentation

<https://touchcue.theaifam5.cc/>

## Getting help

Ask questions and share ideas in [Discussions](https://github.com/theaifam5/touchcue/discussions). Report bugs and request features in [Issues](https://github.com/theaifam5/touchcue/issues).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
