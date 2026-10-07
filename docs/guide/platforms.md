# Platforms

Linux is the only platform touchcue works on. Releases include archives for Windows (`x86_64`) and macOS (`x86_64` and `aarch64`), and CI tests on both, but there touchcue has no sources or output backends and detects nothing: `run`, `list-devices`, `trace` and `gpg` print `not supported on this platform yet` and exit with status 2.

## Linux

- Runs as a user process, in the foreground or as a systemd user service. See [Getting started](./getting-started#as-a-systemd-user-service).
- Detects FIDO keys by reading their `/dev/hidraw*` nodes read-only, alongside browsers and ssh, without taking reports from them. It needs read access to those nodes. touchcue ships no udev rules: the access must come from the system's existing rules, which on most systemd distributions grant the logged-in user access to FIDO security keys through the `uaccess` tag. `touchcue check` shows whether each key is readable.
- Names the requesting application by finding the processes that hold the device open in `/proc`, then their application through their cgroup unit or, failing that, their executable and its desktop entry, skipping hidden entries such as URL handlers. OpenPGP requests are attributed to a client of gpg-agent, preferring gpg and ssh tools. The requester, such as the program that ran `git`, is the first process below the application that is not a shell, multiplexer or wrapper.
- Shows popups through Wayland layer-shell or X11, including XWayland, and notifications through the session D-Bus.

An optional privileged helper, `touchcue-helper`, is planned for exact attribution of root callers, PIV and OATH, and Ledger. The binary exists but does nothing yet.

Planned sources: PIV, OATH, YubiKey HMAC/OTP, fprintd, Trezor and Ledger.

## Windows

Planned.

- FIDO HID access is expected to need a SYSTEM service.
- Other details are under investigation.

## macOS

Planned.

- A signed `.app` with a LaunchAgent is planned.
- Input Monitoring permission may be needed. Under investigation.
