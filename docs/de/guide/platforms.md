# Plattformen

touchcue funktioniert ausschließlich unter Linux. Die Releases enthalten zwar Archive für Windows (`x86_64`) und macOS (`x86_64` und `aarch64`), und die CI testet auch auf beiden Systemen. Dort fehlen touchcue aber Quellen und Ausgabe-Backends, sodass es nichts erkennt: `run`, `list-devices`, `trace` und `gpg` geben `not supported on this platform yet` aus und enden mit dem Status 2.

## Linux

- touchcue läuft als Benutzerprozess, entweder im Vordergrund oder als systemd-Benutzerdienst. Siehe [Erste Schritte](./getting-started#as-a-systemd-user-service).
- FIDO-Schlüssel erkennt es, indem es ihre `/dev/hidraw*`-Knoten ausschließlich lesend öffnet – parallel zu Browsern und ssh und ohne ihnen Reports wegzunehmen. Dafür braucht es Lesezugriff auf diese Knoten. touchcue bringt keine eigenen udev-Regeln mit; der Zugriff muss sich aus den vorhandenen Regeln des Systems ergeben. Auf den meisten systemd-Distributionen erhält der angemeldete Benutzer über das Tag `uaccess` Zugriff auf FIDO-Sicherheitsschlüssel. Ob ein Schlüssel lesbar ist, zeigt `touchcue check`.
- Um die anfragende Anwendung zu benennen, sucht touchcue in `/proc` die Prozesse, die das Gerät geöffnet haben, und ermittelt deren Anwendung über ihre cgroup-Unit. Gelingt das nicht, dient die ausführbare Datei samt zugehörigem Desktop-Eintrag als Grundlage; versteckte Einträge wie URL-Handler werden dabei übersprungen. OpenPGP-Anfragen werden einem Client von gpg-agent zugeordnet, wobei gpg- und ssh-Tools Vorrang haben. Der Urheber – etwa das Programm, das `git` aufgerufen hat – ist der erste Prozess unterhalb der Anwendung, der weder Shell noch Multiplexer noch Wrapper ist.
- Popups erscheinen über Wayland layer-shell oder X11 (einschließlich XWayland), Benachrichtigungen über den Sitzungs-D-Bus.

Geplant ist ein optionaler privilegierter Helfer, `touchcue-helper`: für die exakte Zuordnung von root-Aufrufern, für PIV und OATH sowie für Ledger. Das Binary gibt es bereits, es hat aber noch keine Funktion.

Geplante Quellen: PIV, OATH, YubiKey HMAC/OTP, fprintd, Trezor und Ledger.

## Windows

Geplant.

- Für den Zugriff auf FIDO-HID-Geräte wird voraussichtlich ein SYSTEM-Dienst nötig sein.
- Weitere Details werden noch untersucht.

## macOS

Geplant.

- Vorgesehen ist eine signierte `.app` mit einem LaunchAgent.
- Möglicherweise ist die Berechtigung zur Eingabeüberwachung erforderlich. Das wird noch untersucht.
