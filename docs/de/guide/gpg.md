# gpg und ssh über gpg-agent

touchcue kann die Berührungsaufforderung auch für OpenPGP-Kartenvorgänge anzeigen, die über gpg-agent laufen: Signieren und Entschlüsseln mit `gpg` sowie die ssh-Authentifizierung über die ssh-Unterstützung von gpg-agent. Dazu schaltet sich touchcue zwischen gpg-agent und scdaemon. Das funktioniert nur unter Linux.

## Installation

```sh
touchcue gpg install
```

Der Befehl geht so vor:

1. Er ermittelt das GnuPG-Home-Verzeichnis mit `gpgconf --list-dirs homedir`.
2. Er kopiert `gpg-agent.conf` nach `gpg-agent.conf.touchcue-backup` oder legt `gpg-agent.conf` mit dem Modus 0600 an, falls die Datei fehlt. Eine Sicherung, die von einer früheren Installation noch vorhanden ist, bleibt unverändert und wird nie überschrieben.
3. Er hängt `scdaemon-program <path to touchcue>` an. Die Datei wird dabei atomar ersetzt: Der neue Inhalt landet zunächst in einer temporären Datei im selben Verzeichnis, die anschließend per Umbenennen an die Stelle der alten tritt. Ist `gpg-agent.conf` ein symbolischer Link, wird das Linkziel ersetzt und der Link bleibt bestehen; die Dateiberechtigungen bleiben erhalten.
4. Er führt `gpgconf --reload gpg-agent` und `gpgconf --kill scdaemon` aus, damit beim nächsten Kartenzugriff touchcue gestartet wird.

Setzt `gpg-agent.conf` `scdaemon-program` bereits auf ein anderes Programm, bricht der Befehl ab, ohne etwas zu ändern.

Ist die Zeile schon vorhanden, ändert er nichts. Außer der temporären Datei liest und schreibt er keine weiteren Dateien im GnuPG-Home-Verzeichnis.

Als Pfad wird das touchcue-Binary eingetragen, mit dem Sie den Befehl aufgerufen haben. Wenn Sie das Binary verschieben oder aktualisieren, führen Sie `touchcue gpg uninstall` und danach `touchcue gpg install` mit dem neuen Binary aus. `install` warnt, wenn das Binary in einem Verzeichnis liegt, das bei einem Upgrade ersetzt wird – etwa in einem mise-Installationsverzeichnis, in einer cargo-Registry oder in einem Verzeichnis, dessen Name eine Versionsnummer enthält. Verwenden Sie besser einen festen Pfad wie `~/.cargo/bin` oder `/usr/bin` oder einen Link auf die jeweils aktuelle Version.

## Deinstallation

```sh
touchcue gpg uninstall
```

Der Befehl entfernt die `scdaemon-program`-Zeile, die auf touchcue verweist, und lädt dann wie bei der Installation gpg-agent neu und beendet scdaemon. Die Sicherung bleibt erhalten. Eine Zeile, die auf ein anderes Programm verweist, wird nie entfernt.

`touchcue gpg uninstall --restore-backup` ersetzt `gpg-agent.conf` stattdessen durch die Sicherung und löscht diese anschließend. Weicht `gpg-agent.conf` abgesehen von der touchcue-Zeile von der Sicherung ab, verweigert der Befehl das und ändert nichts, denn die Wiederherstellung würde diese Änderungen verwerfen. Führen Sie in diesem Fall `touchcue gpg uninstall` aus und löschen Sie die Sicherung selbst.

## Funktionsweise

gpg-agent startet sein `scdaemon-program` mit dem Argument `--multi-server`. Wird touchcue auf diese Weise gestartet, arbeitet es als Wrapper:

- Es startet den echten scdaemon aus `gpgconf --list-dirs libexecdir` mit denselben Argumenten und derselben Umgebung. Lässt sich gpgconf nicht ausführen oder schlägt es fehl, probiert touchcue der Reihe nach `/usr/libexec/scdaemon`, `/usr/lib/gnupg/scdaemon` und `/usr/lib/gnupg2/scdaemon`. Ein Programm, das sich als touchcue selbst herausstellt, startet es nie.
- Es kopiert die Assuan-Datenströme zwischen gpg-agent und scdaemon in beide Richtungen Byte für Byte. Die stderr-Ausgabe von scdaemon, also das Protokoll von gpg-agent, wird unverändert durchgereicht. touchcue schreibt dort nur gelegentlich eigene Warnungen hinein, erkennbar am Präfix `touchcue-scdaemon:` – etwa wenn sich der Socket-Proxy nicht einrichten lässt. Unter systemd erscheinen sie im Journal von `gpg-agent.service`; läuft gpg-agent mit `--daemon`, verwirft es die stderr-Ausgabe von scdaemon. Über `TOUCHCUE_LOG` in der Umgebung von gpg-agent lässt sich die Protokollebene ändern.
- Es leitet SIGTERM, SIGINT, SIGHUP, SIGUSR1 und SIGUSR2 an scdaemon weiter und übernimmt beim Beenden dessen Exit-Status.
- Es markiert sich als nicht dumpbar, sodass andere Prozesse desselben Benutzers seinen Speicher nicht auslesen können. Gelingt das nicht, startet er nicht: gpg-agent kann die Karte dann nicht nutzen, und der Grund steht im Log von gpg-agent. Das schützt nur den Wrapper selbst: PINs und Daten durchlaufen auch gpg-agent und den echten scdaemon.

Beim Kopieren beobachtet touchcue die Befehlszeilen. `PKSIGN`, `PKDECRYPT` und `PKAUTH` leiten einen Signier-, Entschlüsselungs- bzw. Authentifizierungsvorgang ein. Antwortet scdaemon danach 400 ms lang nicht, wartet die Karte auf eine Berührung. touchcue meldet das dem Daemon über `$XDG_RUNTIME_DIR/touchcue/helper.sock`. Sobald scdaemon mit `OK` oder `ERR` antwortet, endet die Meldung.

Die PIN-Eingabe zählt nicht als Warten: Fragt scdaemon nach der PIN, hält der Timer an, bis gpg-agent antwortet. Vorgänge, die innerhalb von 400 ms abgeschlossen sind – etwa bei einer zwischengespeicherten Berührung –, werden nicht gemeldet.

Läuft der Daemon nicht, reicht der Wrapper nur durch. Das Melden verzögert oder verändert den Datenstrom in keinem Fall.

## Touch-Richtlinie und Zuordnung

Der Daemon zeigt einen gemeldeten Vorgang nur an, wenn die Touch-Richtlinie (UIF) der Karte für den verwendeten Schlüsselplatz eine Berührung verlangt oder die Richtlinie nicht bekannt ist. Er fragt die Richtlinie bei gpg-agent mit `KEYINFO --list` und `SCD GETATTR UIF-1` bis `UIF-3` ab – und zwar nur, solange kein Kartenvorgang offen ist, da gpg-agent Kartenzugriffe nacheinander abarbeitet. Bis zur ersten Abfrage wird jeder Vorgang angezeigt. Findet der Daemon die Sockets von gpg-agent nicht über `gpgconf`, wird jeder Vorgang angezeigt, aber keiner zugeordnet.

gpg-agent verrät nicht, welcher Client einen Vorgang angefordert hat. touchcue betrachtet deshalb die Prozesse, die mit den Sockets von gpg-agent verbunden sind – ausgenommen gpg-agent, scdaemon und touchcue selbst. Davon wählt es den neuesten `gpg`-, `gpg2`-, `gpgsm`-, `ssh`-, `scp`-, `sftp`-, `ssh-add`- oder `ssh-keygen`-Prozess, sonst den neuesten anderen Prozess, und setzt `request.confidence` auf `medium`. So verdrängt ein Programm mit einer ungenutzten offenen Verbindung, etwa ein Editor, nicht das Tool, das tatsächlich anfragt. Es bleibt trotzdem eine Vermutung: Die Rangfolge verlässt sich auf den Prozessnamen `comm`, den jeder Prozess desselben Benutzers frei setzen kann, zum Beispiel auf `gpg`. Sind zwei Tools gleichzeitig verbunden, wird das neuere genannt. Ausgehend von diesem Client wird anschließend der Urheber ermittelt, wie unter [Platzhalter](./placeholders#requester) beschrieben; auch sein Name ist nur eine Behauptung. Ein `auth`-Vorgang, während ein Client mit dem ssh-Socket von gpg-agent verbunden ist, erhält `request.source` `ssh`, alle anderen Vorgänge erhalten `gpg`. Dem Proxy von yubikey-touch-detector wird ein Vorgang nur dann zugeordnet, wenn kein anderer Client verbunden ist, und zwar mit der Zuverlässigkeit `low`.

## Zuordnung über den PIN-Dialog {#pin-dialog-attribution}

Unter Linux kann touchcue außerdem als pinentry-Wrapper von gpg-agent dienen. Legen Sie dazu einen Link an:

```sh
ln -s "$(command -v touchcue)" ~/.local/bin/touchcue-pinentry
```

Tragen Sie diese Zeile von Hand in `gpg-agent.conf` ein und ersetzen Sie `/home/USER` durch Ihr Home-Verzeichnis:

```text
pinentry-program /home/USER/.local/bin/touchcue-pinentry
```

Laden Sie gpg-agent neu:

```sh
gpgconf --reload gpg-agent
```

Der Wrapper startet `pinentry` aus dem `PATH`. Soll ein anderes pinentry-Programm verwendet werden, setzen Sie `TOUCHCUE_PINENTRY` in der Umgebung von gpg-agent auf dessen absoluten Pfad. Ein Programm, das sich als touchcue selbst herausstellt, wird abgelehnt. Der Wrapper setzt für das echte pinentry `TOUCHCUE_PINENTRY_ACTIVE=1` und startet nicht, wenn diese Variable bereits gesetzt ist. Auch `touchcue pinentry [ARGS]...` startet den Wrapper und reicht die Argumente unausgewertet durch; das erste Argument muss dabei `pinentry` sein.

Der Wrapper kopiert stdin und stdout Byte für Byte, übernimmt die stderr-Ausgabe von pinentry, leitet SIGTERM, SIGINT, SIGHUP, SIGUSR1 und SIGUSR2 weiter und endet mit dem Exit-Status von pinentry. Assuan-Befehle beantwortet er nie selbst. Er markiert sich als nicht dumpbar und startet nicht, wenn das misslingt, sodass der PIN-Dialog fehlschlägt; das schützt nur den Wrapper, nicht gpg-agent oder das echte pinentry.

Auf einer Kopie des Datenstroms wertet er ausschließlich `OPTION owner=<pid>/<uid>`, die Verben `GETPIN` und `CONFIRM` sowie die Antwortklasse `OK` bzw. `ERR` aus. Beschreibungen, Aufforderungstexte, PINs und Datenzeilen werden weitergeleitet, aber nie interpretiert, vom Beobachter gespeichert oder protokolliert. Schickt gpg-agent eine Zeile, die einschließlich Zeilenende länger als 1002 Byte ist, endet die Beobachtung; die Weiterleitung läuft weiter. Meldungen enthalten nur PID und UID des Eigentümers, eine laufende Nummer und das Ergebnis des Dialogs. Läuft kein Daemon, reicht der Wrapper nur durch.

Folgt innerhalb von 30 s auf einen bestätigten PIN- oder Bestätigungsdialog ein OpenPGP-Kartenvorgang, wird dieser dem Client des Dialogs mit `request.confidence` `high` zugeordnet, sofern der Client noch mit gpg-agent verbunden ist. Das gilt nur für den nächsten Vorgang. Ein abgebrochener oder fehlgeschlagener Dialog bewirkt keine Zuordnung. Gibt es keinen solchen Dialog, etwa weil die PIN zwischengespeichert ist, gilt die übliche Rangfolge der Clients.

Um den Wrapper wieder zu entfernen, löschen Sie seine `pinentry-program`-Zeile oder stellen Ihre frühere pinentry-Einstellung von Hand wieder her und laden gpg-agent anschließend neu. `touchcue gpg uninstall` ändert nur die scdaemon-Einstellung.

## Der Socket von scdaemon

gpg-agent kommuniziert auf zwei Wegen mit scdaemon: über die Pipe, mit der es scdaemon gestartet hat, und über den eigenen Socket von scdaemon, `S.scdaemon`, im gpg-Socket-Verzeichnis (`gpgconf --list-dirs socketdir`). Den Socket nutzt gpg-agent, wenn ein weiterer Client die Karte benötigt, während die Pipe belegt ist.

Der Wrapper deckt auch diesen Socket ab. Sobald scdaemon zum ersten Mal über die Pipe geantwortet hat, geht der Wrapper so vor:

1. Er prüft, ob das Socket-Verzeichnis dem Benutzer gehört und den Modus 0700 hat, und verbindet sich einmal mit `S.scdaemon`, um sicherzustellen, dass dort der von ihm gestartete scdaemon lauscht.
2. Er bindet einen eigenen Socket an `S.scdaemon.touchcue`, mit dem Modus 0600.
3. Er tauscht die beiden Pfade mit einer einzigen atomaren Umbenennung aus. So bleibt `S.scdaemon` durchgehend erreichbar, und der Socket von scdaemon liegt danach unter `S.scdaemon.touchcue`.
4. Er nimmt an `S.scdaemon` nur Verbindungen desselben Benutzers an, höchstens 32 gleichzeitig, leitet jede davon Byte für Byte an den Socket von scdaemon weiter und verfolgt sie genauso wie die Pipe.

Beendet sich scdaemon, entfernt der Wrapper beide Pfade – allerdings jeweils nur, solange dort noch der erwartete Socket liegt; der Socket eines neueren scdaemon bleibt also unberührt. Prüfen und Entfernen sind getrennte Schritte; ein Socket, der genau dazwischen gebunden wird, könnte daher trotzdem entfernt werden. Fällt der Proxy aus, während scdaemon noch läuft, tauscht der Wrapper die Pfade zurück.

Lässt sich der Socket nicht über den Proxy führen, protokolliert der Wrapper eine einzelne Warnung, verfolgt nur die Pipe und leitet weiterhin alles durch. Das passiert zum Beispiel, wenn `S.scdaemon` fehlt, kein Socket desselben Benutzers ist oder von einem anderen Prozess bedient wird, oder wenn das Socket-Verzeichnis gemeinsam genutzt wird.
