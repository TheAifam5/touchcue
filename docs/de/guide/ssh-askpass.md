# OpenSSH-Sicherheitsschlüssel

::: v-pre
Für `ed25519-sk`- und `ecdsa-sk`-Schlüssel zeigt die FIDO-Quelle das Berührungs-Popup ohnehin schon an. `touchcue askpass` ergänzt, welcher Schlüssel verwendet wird und – wenn ssh-agent signiert – für welchen entfernten Benutzer. In Vorlagen steht dieser Text als `{{ request.detail }}` zur Verfügung, zum Beispiel `ED25519-SK SHA256:… → user git`. Den Zielhost teilt OpenSSH nicht mit.
:::

`touchcue askpass` erzeugt nie ein eigenes Popup, sondern ergänzt nur das FIDO-Popup für dieselbe Berührung um Details.

## Funktionsweise

Für den Hinweis zur Anwesenheitsprüfung startet OpenSSH das `SSH_ASKPASS`-Programm und schickt ihm `SIGTERM`, sobald die Prüfung endet – unabhängig davon, ob der Schlüssel berührt wurde. `touchcue askpass` meldet den Hinweis an den Daemon und wartet auf dieses Signal.

OpenSSH startet askpass für einen solchen Hinweis nur, wenn `DISPLAY` oder `WAYLAND_DISPLAY` gesetzt ist oder `SSH_ASKPASS_REQUIRE=force` gilt. Ein einfaches `ssh`, dessen Standardfehlerausgabe ein Terminal ist, schreibt den Hinweis stattdessen ins Terminal und startet askpass nicht.

Liegt der Schlüssel bei ssh-agent, schickt der Agent den Hinweis. Die unten genannten Variablen müssen daher in der Umgebung des Agenten gesetzt sein. Läuft er als systemd-Benutzerdienst, verwenden Sie `systemctl --user set-environment` oder ein Drop-in mit `Environment=`.

## Einrichtung

`SSH_ASKPASS` erwartet einen Programmpfad ohne Argumente. touchcue arbeitet als askpass, wenn es über einen Link namens `touchcue-askpass` aufgerufen wird:

```sh
ln -s "$(command -v touchcue)" ~/.local/bin/touchcue-askpass
```

Setzen Sie dann in der Umgebung von ssh bzw. ssh-agent:

```sh
export SSH_ASKPASS="$HOME/.local/bin/touchcue-askpass"
export SSH_ASKPASS_REQUIRE=prefer   # or "force" when DISPLAY/WAYLAND_DISPLAY is not set
export TOUCHCUE_ASKPASS_FALLBACK=/usr/lib/ssh/ssh-askpass
```

`touchcue askpass <message>` bewirkt dasselbe und eignet sich zum Testen; alle Argumente nach `askpass` werden unausgewertet durchgereicht.

Im Daemon muss `[sources.gpg]` aktiviert sein, was standardmäßig der Fall ist; askpass-Meldungen laufen über dessen Helfer-Socket.

## Passphrasen

touchcue liest niemals Passphrasen. Alle anderen askpass-Abfragen – Passphrasen ebenso wie `confirm`-Abfragen – reicht es unverändert an das Programm in `TOUCHCUE_ASKPASS_FALLBACK` weiter, also an Ihr gewohntes askpass wie `ssh-askpass`, `ksshaskpass` oder `lxqt-openssh-askpass`. Ist diese Variable nicht gesetzt, gibt `touchcue askpass` einen Fehler aus und endet mit dem Status 1; OpenSSH wertet das als abgebrochene Abfrage. Das Fallback-Programm darf nicht wieder auf touchcue verweisen.

## Ohne Daemon

Läuft der Daemon nicht, wartet `touchcue askpass` trotzdem auf das Signal von OpenSSH und beendet sich dann stillschweigend; ssh wird also nie blockiert. Kommt 600 Sekunden lang kein Signal, beendet es sich von selbst.
