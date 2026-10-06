# Architecture

## Crates

| Crate | Responsibility |
| --- | --- |
| `touchcue` | The `touchcue` binary: CLI, daemon loop, `check`, `gpg install`, the scdaemon wrapper and `askpass`. |
| `touchcue-core` | Domain model, request state machine, CTAPHID and Assuan parsing, template engine and configuration, with no OS dependencies. |
| `touchcue-detect` | Sources of touch requests: the Linux hidraw watcher for FIDO keys. |
| `touchcue-appinfo` | Resolves a process id to an application name, id and icon. |
| `touchcue-ui` | Popup and notification backends. |
| `touchcue-ipc` | JSON event socket, compatible socket, D-Bus service, the helper socket for gpg and askpass reports, and gpg-agent queries. |
| `touchcue-helper` | Optional privileged helper. A stub; nothing is implemented yet. |

## Runtime

`touchcue run` is one process on a multi-threaded tokio runtime with two worker threads. The hidraw watcher, the helper socket, the IPC endpoints and the UI each run as tasks. The watcher and the helper socket send signals over bounded channels to one daemon loop, which owns the state machine and sends prompts to the UI task and events to the IPC tasks. Blocking work, such as scanning `/proc` to attribute a request, runs on tokio's blocking pool, with at most four threads and a 1 s deadline per attribution. SIGINT and SIGTERM stop every task within fixed deadlines.

The scdaemon wrapper and `touchcue askpass` are separate, short-lived touchcue processes started by gpg-agent and OpenSSH. They report to the daemon over `$XDG_RUNTIME_DIR/touchcue/helper.sock`; the daemon never replies.

## Request lifecycle

A request starts in `waiting` and ends in exactly one of `touched`, `cancelled`, `failed` or `timed_out`. An ended operation lingers for `sources.fido.retry_window_ms`, so that a client retry continues the same request and the prompt can show the outcome, before the request ends.

```
waiting -> touched | cancelled | failed | timed_out
```

## FIDO detection

A FIDO authenticator that waits for a touch sends CTAPHID `KEEPALIVE` (`0xBB`) frames with status `UPNEEDED` (`0x02`). touchcue tracks these per CTAPHID channel ID. A request ends on the final frame of the same channel ID, or when no keepalive arrives for `sources.fido.keepalive_timeout_ms` (1.5 s by default). CTAP2 status codes in the final frame tell a cancel (`0x2D`, `0x27`) or a timeout (`0x2F`, `0x3A`) apart from a touch. For U2F, a reply with status word `0x6985` (conditions not satisfied) means the key waits for a touch.

Tracking per channel ID keeps the prompt steady: frames from other channels do not end or restart the request.

## OpenPGP detection

gpg-agent runs touchcue as its `scdaemon-program`. The wrapper starts the real scdaemon and watches the Assuan commands. A `PKSIGN`, `PKDECRYPT` or `PKAUTH` that gets no reply for 400 ms is reported as waiting for a touch. The daemon reports it only when the card's touch policy (UIF) for that key requires a touch, or is unknown. See [gpg and ssh](./guide/gpg).
