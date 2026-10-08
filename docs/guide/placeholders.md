# Placeholders

Templates in `[templates]` and `[[rules]]` substitute values from the current touch request. `[[rules]]` match on the same names.

## Template syntax

Templates use Jinja syntax, rendered by [MiniJinja](https://docs.rs/minijinja/2/minijinja/syntax/index.html). Earlier versions of touchcue used single braces, as in `{app.name|"An application"}`; such a template is now a configuration error. A placeholder `namespace.field` from the table below is written as an attribute, such as `app.name`:

```text
{{ request.method }}                               the value of a placeholder
{{ app.name or process.name or "An application" }} the first value that is not empty, else the literal
{{ requester.name }}{% if app.name %} ({{ app.name }}){% endif %}
                                                   text only when a value is not empty
{% if request.method == "openpgp" %}…{% else %}…{% endif %}
                                                   a branch
{{ requester.name | upper }}                       a filter
{%- if app.name %} … {% endif -%}                  - removes the whitespace before or after a block
{% raw %}{{ }}{% endraw %}                         literal braces
```

::: v-pre
- Every value is a string. A value that is not known is undefined: it renders empty and is false, and so is an empty string. `app.name` renders empty when there is no application.
- Comparisons compare strings, as in `request.count == "2"`. Convert with `int` to compare numbers: `request.count | int > 1`. A comparison printed directly renders `True` or `False`.
- The built-in [filters](https://docs.rs/minijinja/2/minijinja/filters/index.html) and [tests](https://docs.rs/minijinja/2/minijinja/tests/index.html) are available, for example the filters `upper`, `lower`, `title`, `capitalize`, `trim`, `replace`, `default`, `first`, `length` and `int`, and the tests `defined`, `startingwith`, `endingwith` and `in`. The filters `format`, `indent`, `slice` and `batch` are not available. Of the global functions, `range`, `dict` and `namespace` are available. Methods, as in `app.name.upper()`, are not: use a filter.
- `{% set %}`, `{% with %}`, `{% for %}` and `{% filter %}` work. Macros, `include`, `extends` and `import` do not: they are syntax errors.
- Whitespace is kept as written, except that one trailing newline of the template is removed. A `-` inside a block's delimiter, as in `{%-`, `-%}`, `{{-` or `-}}`, removes the whitespace, newlines included, on that side of the block; this helps in templates written as multi-line TOML strings.
- The output is plain text and is not HTML-escaped.
:::

::: v-pre
When the configuration is loaded, a syntax error or an unknown name is a configuration error that points at the offending text. A name is known when it is a placeholder written `namespace.field`, a name the template sets with `set`, `with` or `for`, or one of the global functions above. A whole namespace, as in `{{ app }}` or `app["name"]`, is unknown: write `app.name`.
:::

A failure that only shows while rendering does not lose the prompt. Such failures are an unknown filter or test, an operation on values of the wrong type as in `request.count + 1`, a render that runs more than 1,000 instructions or writes more than 64 KiB, and a render that takes longer than 200 ms. A failed rule template is replaced by the `[templates]` template, and that by the built-in default; a render that takes too long uses the built-in defaults. The daemon logs the failed template's name and the error, at most once a minute per template.

Templates are trusted configuration, written by the owner of the configuration file. The limits above bound the number of instructions and the output of a render, not the time or memory a single instruction takes: an expression such as `"x" * 100000000` builds a string of 100 MB, and joining strings with `~` in a loop can double one at each step. A render that does not finish keeps running in the background; until it ends, every prompt uses the built-in templates and the daemon logs a warning at most once a minute.

## Names

| Namespace | Fields |
| --- | --- |
| `app` | `name`, `id`, `icon`, `exe`, `pid`, `cmdline`, `wm_class`, `container` |
| `process` | `name`, `exe`, `pid`, `cmdline`, `uid`, `chain` |
| `requester` | `name`, `exe`, `pid`, `label` |
| `device` | `vendor`, `model`, `product`, `vid`, `pid`, `kind`, `transport` |
| `request` | `method`, `op`, `source`, `class`, `confidence`, `elapsed`, `count`, `state`, `detail` |

::: v-pre
`process` is the client process that talks to the device or to gpg-agent, such as `gpg` or `ssh-sk-helper`. `requester` is the program that asked it to, such as `claude` running `git commit -S`. `app` is the desktop application they run in, such as Kitty. A value that is not known is empty, so give a fallback, as in `{{ requester.label or process.name or "An application" }}`.
:::

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
