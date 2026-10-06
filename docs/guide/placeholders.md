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
| `process` | `name`, `exe`, `pid`, `cmdline`, `uid` |
| `device` | `vendor`, `model`, `product`, `vid`, `pid`, `kind`, `transport` |
| `request` | `method`, `op`, `source`, `class`, `confidence`, `elapsed`, `count`, `state`, `detail` |

`app` is the application the request is attributed to, and `process` the client process. A value that is not known is empty, so give a fallback, as in `{app.name|process.name|"An application"}`.

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

`request.confidence` is `high` when the application holds the FIDO device open, `medium` when it is the newest client of gpg-agent at the time of an OpenPGP request, and `low` otherwise.

`request.detail` is empty unless a reporter sent it. `touchcue askpass` sets it for OpenSSH security keys, for example `ED25519-SK SHA256:… → user git`; see [OpenSSH security keys](./ssh-askpass).

There is no `device.serial`.
