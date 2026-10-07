# Placeholders

Templates in `[templates]` and `[[rules]]` substitute values from the current touch request. `[[rules]]` match on the same names.

## Grammar

```text
{path}               value at path
{a|b|"literal"}      first of a, b that has a non-empty value, otherwise the literal
{{ and }}            a literal { and }
```

Inside a literal, `\"` and `\\` are the only escapes. An unknown name or a syntax error is a configuration error.

## Names

| Namespace | Fields |
| --- | --- |
| `app` | `name`, `id`, `icon`, `exe`, `pid`, `cmdline`, `wm_class`, `container` |
| `process` | `name`, `exe`, `pid`, `cmdline`, `uid`, `chain` |
| `requester` | `name`, `exe`, `pid`, `label` |
| `device` | `vendor`, `model`, `product`, `vid`, `pid`, `kind`, `transport` |
| `request` | `method`, `op`, `source`, `class`, `confidence`, `elapsed`, `count`, `state`, `detail` |

`process` is the client process that talks to the device or to gpg-agent, such as `gpg` or `ssh-sk-helper`. `requester` is the program that asked it to, such as `claude` running `git commit -S`. `app` is the desktop application they run in, such as Kitty. A value that is not known is empty, so give a fallback, as in `{requester.label|process.name|"An application"}`.

`process.name` is the client's `comm`. `requester.name` and the names in `process.chain` are the process's `comm`, else its executable's file name. `process.chain` lists the names from the client up to the application's process, client first, for example `gpg ← git ← bash ← claude ← nu ← kitty`: at most 8 names of at most 32 characters each. A longer chain keeps the first 7 names and the last, with `…` between them, as in `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← … ← kitty`.

## Requester

The requester is the program you started inside the application: touchcue walks the parent processes from the application's process down to the client and takes the first process that is not on the skip list. By default the skip list holds shells, terminal multiplexers, command wrappers such as `sudo`, `env` and `timeout`, and service managers. The application's own processes, those running its executable, and processes of other users are always passed over, and when every process is passed over, the requester is the client, or nobody when the client is the application itself. [`requester.skip`](./configuration#requester) replaces the list, `requester.extend_skip` adds to it, and `touchcue check` prints the list and how it reads its own terminal.

| Process tree, client first | `requester.label` |
| --- | --- |
| `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← nu ← kitty`, Claude Code in herdr | `claude in kitty` |
| `gpg ← git ← zsh ← kitty`, `git commit -S` typed in a shell | `git in kitty` |
| `ssh-sk-helper ← ssh ← zsh ← kitty`, `ssh` typed in a shell | `ssh in kitty` |
| `gpg ← git ← sh ← make ← bash ← claude ← nu ← kitty` | `claude in kitty` |
| `gpg ← jj ← claude ← nu ← kitty`, or `pass` in place of `jj` | `claude in kitty` |
| `gpg ← git ← release.sh ← zsh ← kitty`, a script run by `bash` | `release.sh in kitty` |
| `gpg ← timeout ← zsh ← kitty` | `gpg in kitty` |
| `git ← nu ← code ← code`, the terminal of Visual Studio Code | `git in Visual Studio Code` |
| `firefox`, holding the FIDO device itself | `Firefox` |
| `ssh ← restic ← backup.sh ← systemd`, without an application | `backup.sh` |
| `sudo ← zsh ← kitty`, with `sudo` as the client | `sudo in kitty` |
| `gpg ← git ← claude ← zsh ← nvim ← zsh ← kitty`, Claude Code in a Neovim terminal | `nvim in kitty`; with `extend_skip = ["nvim"]`, `claude in kitty` |

Names are matched against the skip list by `comm`, else by the executable's file name when `comm` cannot be read. Matching is exact and case-sensitive, an entry ending in `*` is a prefix, and `comm` holds at most 15 bytes, so a longer name needs a prefix entry. `requester.*` is empty when there is no requester; when the application is found through its systemd unit, the application end of the walk is the topmost process of that unit, the one that started the application. The walk stops at the first process whose names cannot be read or whose pid was reused; the requester is then the best guess among the processes read before it.

`requester.label` combines both names: `<requester.name> in <app.name>` when both are known and differ other than in ASCII case, otherwise whichever of `app.name` and `requester.name` is known, and empty when neither is. It is at most 260 characters. Like the names it is built from, it is published to IPC clients and hooks.

Limits:

- A program that runs a terminal, such as Neovim's `:terminal`, is named instead of what runs in that terminal. Add it to `extend_skip`.
- A script keeps its own name only when it is run through its shebang line, as `./release.sh`, and that line names the interpreter directly, as `#!/bin/bash`. Run as `bash release.sh`, or with `#!/usr/bin/env bash`, its `comm` is `bash` and it is skipped like a shell.
- A daemonized tmux server is reparented away from the terminal, so commands run inside it are found without the application.
- Like `app`, the requester is a claim: any process of the same user can choose its `comm`, for example to be skipped, and the request is then attributed to the process above it.

## Values

| Name | Values |
| --- | --- |
| `request.method` | `fido2`, `u2f`, `openpgp` |
| `request.op` | `sign`, `decrypt`, `auth` for OpenPGP; empty for FIDO |
| `request.source` | `fido`; for OpenPGP `ssh` when an `auth` operation runs while a client is connected to gpg-agent's ssh socket, else `gpg` |
| `request.class` | `asserted` for FIDO, `activity` for OpenPGP |
| `request.state` | `waiting`, `touched`, `cancelled`, `failed`, `timed_out` |
| `request.confidence` | `high`, `medium`, `low` |
| `request.elapsed` | whole seconds since the request started |
| `request.count` | number of client attempts merged into this request, starting at 1 |
| `request.detail` | extra text from a reporter, at most 200 characters |
| `device.kind` | `fido`, `openpgp` |
| `device.transport` | `usb`, `bluetooth`, `nfc`, `other` |
| `device.vid`, `device.pid` | USB vendor and product id, four lowercase hex digits |
| `device.vendor` | vendor name, for example `Yubico`; for an OpenPGP card, its manufacturer |

`request.confidence` is `high` when one process holds the FIDO device open, `medium` when several do or when the client is chosen among the clients of gpg-agent for an OpenPGP request, and `low` otherwise.

`request.detail` is empty unless a reporter sent it. `touchcue askpass` sets it for OpenSSH security keys, for example `ED25519-SK SHA256:… → user git`; see [OpenSSH security keys](./ssh-askpass).

There is no `device.serial`.
