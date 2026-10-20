# Architektur

## Crates

| Crate | Zuständigkeit |
| --- | --- |
| `touchcue` | Das Binary `touchcue`: CLI, Daemon-Schleife, `check`, `gpg install`, der scdaemon-Wrapper, `askpass` und der pinentry-Wrapper. |
| `touchcue-core` | Domänenmodell, Zustandsautomat für Anfragen, CTAPHID- und Assuan-Parser, Vorlagen für Aufforderungen (MiniJinja) und Konfiguration – ohne Abhängigkeiten vom Betriebssystem. |
| `touchcue-detect` | Quellen für Berührungsanfragen: der hidraw-Watcher unter Linux für FIDO-Schlüssel. |
| `touchcue-appinfo` | Ermittelt zu einer Prozess-ID den Anwendungsnamen, die Anwendungs-ID und das Symbol sowie unter den Vorfahren den Urheber. Sucht Symbole in Symbolthemen und liest das Symbolthema aus den Einstellungsdateien des Desktops. |
| `touchcue-ui` | Backends für Popups und Benachrichtigungen. |
| `touchcue-ipc` | JSON-Ereignis-Socket, kompatibler Socket, D-Bus-Dienst, der Helfer-Socket für Meldungen von gpg, askpass und pinentry, Abfragen an gpg-agent sowie die Einstellungen des Desktop-Portals. |
| `touchcue-hooks` | Führt die `[[hooks]]`-Befehle bei Ereignissen aus – bei Hooks mit `until` einen Prozess pro Anfrage –, jeweils ohne Shell und in einer eigenen Prozessgruppe. |
| `touchcue-helper` | Optionaler privilegierter Helfer. Bisher nur ein Platzhalter; implementiert ist noch nichts. |

## Laufzeit

`touchcue run` ist ein einzelner Prozess auf einer mehrthreadigen tokio-Laufzeit mit zwei Worker-Threads. Der hidraw-Watcher, der Helfer-Socket, die IPC-Endpunkte und die UI laufen jeweils als eigener Task. Watcher und Helfer-Socket schicken ihre Signale über begrenzte Kanäle an eine zentrale Daemon-Schleife. Sie verwaltet den Zustandsautomaten und gibt Aufforderungen an den UI-Task sowie Ereignisse an die IPC-Tasks und die Hooks weiter. Jeder Hook hat einen eigenen Task mit begrenzter Warteschlange, der seine Befehle startet. Bei einem Hook mit `until` ist zusätzlich die stdin-Warteschlange jedes seiner Prozesse begrenzt, und es laufen höchstens 8 solcher Prozesse gleichzeitig. Die Daemon-Schleife wartet nie auf sie. Blockierende Arbeiten wie das Durchsuchen von `/proc`, um eine Anfrage zuzuordnen, oder das Nachschlagen eines Regelsymbols laufen im Blocking-Pool von tokio. Dieser hat höchstens vier Threads; eine Zuordnung darf 1 s dauern, eine Symbolsuche 0,5 s. Auf SIGINT und SIGTERM beenden sich alle Tasks innerhalb fester Fristen.

Der scdaemon-Wrapper, `touchcue askpass` und `touchcue pinentry` laufen als separate touchcue-Prozesse, die gpg-agent bzw. OpenSSH starten. Sie melden sich über `$XDG_RUNTIME_DIR/touchcue/helper.sock` beim Daemon; der Daemon antwortet ihnen nie.

## Lebenszyklus einer Anfrage

Jede Anfrage beginnt im Zustand `waiting` und endet in genau einem der Zustände `touched`, `cancelled`, `failed` oder `timed_out`. Ist ein Vorgang beendet, bleibt er noch `sources.fido.retry_window_ms` lang erhalten. Versucht der Client es in dieser Zeit erneut, läuft dieselbe Anfrage weiter; währenddessen zeigt die Aufforderung das Ergebnis an.

```
waiting -> touched | cancelled | failed | timed_out
```

## FIDO-Erkennung

Wartet ein FIDO-Authenticator auf eine Berührung, sendet er CTAPHID-`KEEPALIVE`-Frames (`0xBB`) mit dem Status `UPNEEDED` (`0x02`). touchcue verfolgt diese Frames getrennt nach CTAPHID-Kanal-ID. Eine Anfrage endet mit dem abschließenden Frame derselben Kanal-ID oder wenn `sources.fido.keepalive_timeout_ms` lang (standardmäßig 1,5 s) kein Keepalive mehr eintrifft. Anhand der CTAP2-Statuscodes im abschließenden Frame lässt sich ein Abbruch (`0x2D`, `0x27`) oder eine Zeitüberschreitung (`0x2F`, `0x3A`) von einer Berührung unterscheiden. Bei U2F zeigt eine Antwort mit dem Statuswort `0x6985` (Bedingungen nicht erfüllt) an, dass der Schlüssel auf eine Berührung wartet.

Frames mit anderen CTAPHID-Kanal-IDs beenden die Anfrage weder, noch starten sie sie neu.

## OpenPGP-Erkennung

gpg-agent startet touchcue als sein `scdaemon-program`. Der Wrapper startet seinerseits den echten scdaemon und beobachtet die Assuan-Befehle. Bleibt ein `PKSIGN`, `PKDECRYPT` oder `PKAUTH` 400 ms lang unbeantwortet, meldet der Wrapper, dass auf eine Berührung gewartet wird. Der Daemon zeigt das nur an, wenn die Touch-Richtlinie (UIF) der Karte für diesen Schlüssel eine Berührung verlangt oder nicht bekannt ist. Siehe [gpg und ssh](./guide/gpg).

## Zuordnung

Bei einer FIDO-Anfrage sucht der Daemon in `/proc` nach den Prozessen, die den Geräteknoten geöffnet haben, bei einer OpenPGP-Anfrage nach den Prozessen, die mit den Sockets von gpg-agent verbunden sind. Der erste davon ist der Client, `process`. Unter den Clients von gpg-agent haben Tools wie `gpg` und `ssh` Vorrang, danach neuere Prozesse; die Zuverlässigkeit ist dann `medium`. Hat `touchcue pinentry` einen bestätigten PIN- oder Bestätigungsdialog gemeldet, dessen gpg-agent-Client zu den verbundenen Prozessen dieses Benutzers gehört, gilt für den nächsten OpenPGP-Kartenvorgang innerhalb von 30 s stattdessen dieser Prozess als Client, mit der Zuverlässigkeit `high`. Ein abgebrochener oder fehlgeschlagener Dialog ändert nichts, und eine bereits zugeordnete Anfrage behält ihren Client. Vom Client aus geht touchcue einmal die Elternprozesse nach oben durch, höchstens 32, und ermittelt dabei:

- die Anwendung, `app`: den nächstgelegenen Prozess in einer systemd-Anwendungs-Unit, sonst den nächstgelegenen, dessen ausführbare Datei zu einem Desktop-Eintrag passt;
- den Urheber, `requester`: auf dem Weg von der Anwendung hinunter zum Client den ersten Prozess, der nicht auf der Ausschlussliste der Shells, Multiplexer, Wrapper und Dienstmanager steht, nicht die ausführbare Datei der Anwendung ausführt und keinem anderen Benutzer gehört;
- die Kette, `process.chain`: die Namen vom Client hinauf bis zur Anwendung.

Der Durchlauf bricht ab, wenn sich die Startzeit eines Prozesses zwischen der ersten Prüfung und der erneuten Prüfung nach dem Lesen seiner Namen ändert. Die Regeln und ihre Grenzen beschreibt [Platzhalter](./guide/placeholders#requester).
