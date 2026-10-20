# Erste Schritte

::: warning Frühe Entwicklungsphase
touchcue läuft bisher nur unter Linux. Für Windows und macOS gibt es zwar Release-Archive, sie erkennen aber noch nichts; siehe [Plattformen](./platforms).
:::

## Was touchcue macht

Wartet ein Hardware-Authenticator darauf, physisch berührt zu werden, zeigt touchcue ein Popup an, das keinen Eingabefokus an sich zieht, oder eine Desktop-Benachrichtigung. Die Aufforderung nennt das Gerät und das Programm, das die Berührung angefordert hat – etwa den Browser oder das Tool, das `git` aufgerufen hat. Erkannt werden:

- FIDO2/U2F-Schlüssel beliebiger Hersteller, die über ihre hidraw-Geräte ausgelesen werden;
- OpenPGP-Karten, die über gpg-agent genutzt werden: von `gpg` und von ssh über die ssh-Unterstützung von gpg-agent. Dafür ist [`touchcue gpg install`](./gpg) erforderlich.

## Installation

### Aus einem Release-Archiv

Jedes [Release](https://github.com/theaifam5/touchcue/releases) enthält pro Plattform ein Archiv `touchcue-<target>.tar.gz` sowie eine Datei `SHA256SUMS`. Für Linux auf x86_64 mit glibc:

```sh
curl -LO https://github.com/theaifam5/touchcue/releases/latest/download/touchcue-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/theaifam5/touchcue/releases/latest/download/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf touchcue-x86_64-unknown-linux-gnu.tar.gz
cd touchcue-x86_64-unknown-linux-gnu
```

Linux-Archive gibt es für `x86_64` und `aarch64`, jeweils in einer `gnu`- und einer `musl`-Variante. Jedes Archiv enthält `touchcue`, `touchcue-helper`, `completions/`, `man/touchcue.1`, die Lizenzen und die README. Installieren Sie das Binary, die Manpage und die Shell-Vervollständigung für Ihre Shell:

```sh
install -D touchcue ~/.local/bin/touchcue
install -Dm644 man/touchcue.1 ~/.local/share/man/man1/touchcue.1
install -Dm644 completions/touchcue.bash ~/.local/share/bash-completion/completions/touchcue
install -Dm644 completions/touchcue.fish ~/.config/fish/completions/touchcue.fish
```

In `completions/` liegen außerdem `touchcue.zsh`, `touchcue.nu` und `touchcue.ps1`. Die Vervollständigung setzt die CLI [`usage`](https://usage.jdx.dev/cli/completions) im `PATH` voraus; unter bash muss zudem bash-completion vorher geladen sein. `touchcue-helper` hat noch keine Funktion und muss nicht installiert werden.

### Mit mise

```sh
mise use -g packslip:github.com/theaifam5/touchcue
```

Ist in Ihrer mise-Konfiguration `minimum_release_age` gesetzt, werden Releases, die jünger als dieses Mindestalter sind, unter Umständen ausgeblendet.

### Aus dem Quellcode

Installieren Sie die in `rust-toolchain.toml` festgelegte Rust-Toolchain und führen Sie dann im Repository Folgendes aus:

```sh
cargo install --locked --path crates/touchcue
```

Damit wird `touchcue` nach `~/.cargo/bin` installiert. Wer am Projekt mitarbeitet, kann stattdessen `mise run build` verwenden; das baut alle Crates des Workspace nach `target/`. Die Verzeichnisse `completions/` und `man/` im Repository enthalten dieselben Dateien wie die Archive.

## Einrichtung prüfen

```sh
touchcue check
```

Der Befehl zeigt an:

- ob die Konfiguration gültig ist;
- ob ein Wayland-Compositor, Wayland layer-shell und ein Benachrichtigungsdienst verfügbar sind;
- alle FIDO-Geräte und ob touchcue sie lesen kann;
- welches Ausgabe-Backend `touchcue run` wählen würde;
- die IPC-Endpunkte und die gpg-Einrichtung;
- die Anzahl der Hooks und wie viele davon `until` verwenden;
- die Ausschlussliste für die Urheber-Ermittlung;
- das [Symbolthema](./configuration#icons) samt Herkunft.

Die `requester`-Zeilen zeigen, wie touchcue das Terminal auswertet, in dem es selbst läuft; übersprungene Prozesse stehen in eckigen Klammern:

```text
requester: skip = ["sh", "bash", …, "systemd", "init"] (defaults)
requester: here: touchcue ← [nu] ← [herdr] ← [herdr] ← [nu] ← kitty → "touchcue in kitty"
icons: theme = "Papirus-Dark" (portal org.gnome.desktop.interface)
```

Ist die Konfiguration ungültig oder lässt sich ein FIDO-Gerät nicht lesen, endet der Befehl mit dem Status 1.

`touchcue list-devices` listet die angeschlossenen FIDO-Geräte auf. `touchcue trace` gibt die Erkennungssignale fortlaufend aus, bis Sie es abbrechen – hilfreich, wenn eine Berührung nicht erkannt wird.

## Den Daemon starten

Im Vordergrund:

```sh
touchcue run -v
```

Mit `-v` wird auf Info-Ebene protokolliert, mit `-vv` auf Debug-Ebene. Die Umgebungsvariable `TOUCHCUE_LOG` nimmt einen `tracing`-Filter entgegen und hat Vorrang vor beiden Optionen. Die Protokollausgabe und `touchcue check` sind nur in einem Terminal farbig; setzen Sie `NO_COLOR` auf einen beliebigen nicht leeren Wert, um die Farben abzuschalten.

### Als systemd-Benutzerdienst {#as-a-systemd-user-service}

Jedes Release enthält außerdem die systemd-Unit `touchcue.service`:

```sh
mkdir -p ~/.config/systemd/user
curl -Lo ~/.config/systemd/user/touchcue.service \
  https://github.com/theaifam5/touchcue/releases/latest/download/touchcue.service
```

Die Unit führt `touchcue run` aus; systemd findet das Binary dabei nur in `/usr/local/bin` und `/usr/bin`. Liegt es anderswo, überschreiben Sie den Befehl mit `systemctl --user edit touchcue.service`, zum Beispiel für `~/.local/bin`:

```ini
[Service]
ExecStart=
ExecStart=%h/.local/bin/touchcue run
```

Nach `cargo install` verwenden Sie `%h/.cargo/bin/touchcue`. Bei mise gibt `mise which touchcue` den Pfad aus; er ändert sich allerdings mit jedem Upgrade. Starten Sie den Dienst anschließend:

```sh
systemctl --user daemon-reload
systemctl --user enable --now touchcue.service
```

Die Unit startet zusammen mit `graphical-session.target`. Sie benötigt daher einen Sitzungsmanager, der dieses Target aktiviert.

## gpg und ssh über gpg-agent

```sh
touchcue gpg install
```

Danach startet gpg-agent touchcue als sein `scdaemon-program`. Siehe [gpg und ssh](./gpg). Für `ed25519-sk`- und `ecdsa-sk`-Schlüssel zeigt die FIDO-Quelle die Aufforderung ohnehin schon an; mit [ssh askpass](./ssh-askpass) kommen der verwendete Schlüssel und der entfernte Benutzer hinzu.

## Umstieg von yubikey-touch-detector

1. Stoppen und deaktivieren Sie den Socket und den Dienst von yubikey-touch-detector: `systemctl --user disable --now yubikey-touch-detector.socket yubikey-touch-detector.service`. Bleibt die Socket-Unit aktiv, belegt sie weiterhin den Socket-Pfad, und touchcue verzichtet auf seinen kompatiblen Socket.
2. Starten Sie touchcue wie oben beschrieben.
3. Liest eine Statusleiste oder ein Skript `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, schalten Sie den kompatiblen Socket ein:

   ```toml
   [compat.maxbaz_socket]
   enabled = true
   ```

touchcue stellt diesen Socket unter demselben Pfad und mit denselben 5-Byte-Nachrichten bereit: `U2F_1`/`U2F_0`, `GPG_1`/`GPG_0` und `MAC_1`/`MAC_0`. Solange yubikey-touch-detector den Socket noch bedient, lässt touchcue ihn mit einer Warnung aus. Siehe [IPC](./configuration#ipc).

## Nächste Schritte

- [Konfiguration](./configuration)
- [Platzhalter](./placeholders)
- [CLI-Referenz](/de/reference/cli/)
