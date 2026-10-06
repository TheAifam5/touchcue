# gpg and ssh through gpg-agent

touchcue can show the touch prompt for OpenPGP card operations that gpg-agent runs: `gpg` signing and decryption, and ssh authentication through gpg-agent's ssh support. It does this by running between gpg-agent and scdaemon. This works on Linux only.

## Install

```sh
touchcue gpg install
```

The command:

1. finds the GnuPG home directory with `gpgconf --list-dirs homedir`;
2. copies `gpg-agent.conf` to `gpg-agent.conf.touchcue-backup`, or creates `gpg-agent.conf` with mode 0600 if it does not exist. A backup that already exists, from an earlier install, is kept as it is and never overwritten;
3. appends `scdaemon-program <path to touchcue>`. The file is replaced atomically: the new contents are written to a temporary file in the same directory, which is then renamed over it. If `gpg-agent.conf` is a symlink, its target is replaced and the link kept, and the file keeps its permissions;
4. runs `gpgconf --reload gpg-agent` and `gpgconf --kill scdaemon`, so the next card access starts touchcue.

It refuses to continue, and changes nothing, when `gpg-agent.conf` already sets `scdaemon-program` to another program.

If the line is already present, it changes nothing. Apart from the temporary file, it reads and writes no other file in the GnuPG home directory.

The configured path is the touchcue binary you ran the command with. After moving or upgrading the binary, run `touchcue gpg uninstall` and then `touchcue gpg install` with the new binary. `install` warns when the binary lies in a directory that an upgrade replaces, such as a mise install directory, a cargo registry, or any directory whose name holds a version number. Prefer a stable path such as `~/.cargo/bin` or `/usr/bin`, or a link to the current version.

## Uninstall

```sh
touchcue gpg uninstall
```

The command removes the `scdaemon-program` line that names touchcue, then reloads gpg-agent and stops scdaemon the same way. It keeps the backup. A line that names another program is never removed.

`touchcue gpg uninstall --restore-backup` replaces `gpg-agent.conf` with the backup instead, and then deletes the backup. It refuses, and changes nothing, when `gpg-agent.conf` without the touchcue line differs from the backup, since restoring would discard those changes; run `touchcue gpg uninstall` and delete the backup instead.

## How it works

gpg-agent starts its `scdaemon-program` with the argument `--multi-server`. When touchcue is started that way, it acts as a wrapper:

- It starts the real scdaemon from `gpgconf --list-dirs libexecdir` with the same arguments and environment. If gpgconf cannot run or fails, it tries `/usr/libexec/scdaemon`, `/usr/lib/gnupg/scdaemon` and `/usr/lib/gnupg2/scdaemon` in that order. It never starts a program that resolves to touchcue itself.
- It copies the Assuan streams between gpg-agent and scdaemon byte for byte, in both directions. scdaemon's stderr, which is gpg-agent's log, goes through unchanged. touchcue adds only its own rare warnings there, prefixed with `touchcue-scdaemon:`, such as when the socket proxy cannot be set up. Under systemd they appear in the journal of `gpg-agent.service`; when gpg-agent runs with `--daemon`, it discards scdaemon's stderr. Set `TOUCHCUE_LOG` in gpg-agent's environment to change the level.
- It forwards SIGTERM, SIGINT, SIGHUP, SIGUSR1 and SIGUSR2 to scdaemon and exits with scdaemon's exit status.
- It makes itself non-dumpable, so other processes of the same user cannot read its memory. This hardens only the wrapper: PINs and data also pass through gpg-agent and the real scdaemon.

While copying, it watches the command lines. `PKSIGN`, `PKDECRYPT` and `PKAUTH` start a sign, decrypt or auth operation. When scdaemon then stays silent for 400 ms, the card is waiting for a touch. touchcue reports this to the daemon over `$XDG_RUNTIME_DIR/touchcue/helper.sock`. scdaemon's `OK` or `ERR` reply ends the report.

PIN entry does not count as waiting. When scdaemon asks for the PIN, the timer stops until gpg-agent answers. Operations that finish within 400 ms, for example with a cached touch, are not reported.

If the daemon is not running, the wrapper only forwards. Reporting never delays or changes the stream.

## Touch policy and attribution

The daemon shows a reported operation only when the card's touch policy (UIF) for the key slot in use requires a touch, or when the policy is unknown. It reads the policy from gpg-agent with `KEYINFO --list` and `SCD GETATTR UIF-1` to `UIF-3`, and only while no card operation is open, since gpg-agent serializes card access. Until the first read, every operation is shown. When the daemon cannot find gpg-agent's sockets through `gpgconf`, every operation is shown and none is attributed.

gpg-agent does not say which client asked for an operation. touchcue attributes it to the newest process connected to gpg-agent's sockets, other than gpg-agent, scdaemon and touchcue, with `request.confidence` `medium`. An `auth` operation while a client is connected to gpg-agent's ssh socket has `request.source` `ssh`, else operations have `gpg`. The proxy of yubikey-touch-detector is attributed, with `low` confidence, only when no other client is connected.

## scdaemon's socket

gpg-agent talks to scdaemon over two paths: the pipe it started scdaemon with, and scdaemon's own socket `S.scdaemon` in the gpg socket directory (`gpgconf --list-dirs socketdir`). gpg-agent uses the socket when another client needs the card while the pipe is in use.

The wrapper covers the socket as well. After scdaemon first answers on the pipe, the wrapper:

1. checks that the socket directory belongs to the user and has mode 0700, and connects once to `S.scdaemon` to check that the scdaemon it started is the process listening there;
2. binds its own socket at `S.scdaemon.touchcue`, with mode 0600;
3. swaps the two paths in one atomic rename, so `S.scdaemon` stays connectable throughout and scdaemon's socket ends up at `S.scdaemon.touchcue`;
4. accepts connections on `S.scdaemon` only from the same user, at most 32 at a time, and forwards each one byte for byte to scdaemon's socket, tracking it like the pipe.

When scdaemon exits, the wrapper removes both paths, but only while each still holds the socket it expects, so a newer scdaemon's socket is left alone. The check and the removal are separate steps, so a socket bound in the moment between them could still be removed. If the proxy fails while scdaemon runs, the wrapper swaps the paths back.

If the socket cannot be proxied, the wrapper logs one warning, tracks only the pipe and forwards everything as before. That happens, for example, when `S.scdaemon` is missing, is not a socket of the same user, or is served by another process, or when the socket directory is shared.
