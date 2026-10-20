# Hooks

Ein Hook führt einen Befehl Ihrer Wahl aus, sobald in touchcue ein bestimmtes Ereignis eintritt – zum Beispiel, um beim Start einer Anfrage einen Ton abzuspielen. Hooks sind `[[hooks]]`-Einträge in der [Konfiguration](./configuration#hooks). Sie funktionieren unter Linux.

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

`touchcue check` zeigt an, wie viele Hooks konfiguriert sind und wie viele davon `until` verwenden.

Ein Hook mit `until = "ended"` wird nicht einmal pro Ereignis ausgeführt, sondern startet für jede Anfrage einen Prozess, der so lange läuft wie die Anfrage. Auf diese Weise zeigen Launcher wie rofi oder fuzzel die Aufforderung an; siehe [Hooks für die Dauer einer Anfrage](#hooks-that-last-for-a-request) und [Launcher](./launchers).

Durch Hooks führt die Konfigurationsdatei Befehle unter Ihrem Benutzerkonto aus. Sorgen Sie deshalb dafür, dass nur Sie die Datei bearbeiten können, und starten Sie den Daemon nie als root mit einer Konfigurationsdatei, die ein anderer Benutzer beschreiben kann.

## Ereignisse

| Ereignis | Wann |
| --- | --- |
| `started` | Eine Anfrage hat begonnen. |
| `updated` | Eine Anfrage hat sich geändert, etwa ihr Zustand, ihre Client-Prozesse oder ihr Detailtext. |
| `ended` | Eine Anfrage ist beendet. |
| `waiting` | Eine Anfrage hat begonnen oder wurde reaktiviert und wartet auf eine Berührung. |
| `lingering` | Der Vorgang einer Anfrage ist ohne Berührung zu Ende gegangen. Die Anfrage bleibt noch `retry_window_ms` lang bestehen, falls der Client es erneut versucht. |
| `revived` | Ein Client hat eine Anfrage in der Nachlaufzeit erneut versucht; sie wartet wieder. |
| `touched` | Der Schlüssel wurde berührt. |
| `cancelled` | Der Client hat den Vorgang abgebrochen. |
| `failed` | Der Vorgang ist fehlgeschlagen. |
| `timed_out` | Der Schlüssel meldet die Anfrage nicht mehr, oder für den Vorgang ist eine Zeitüberschreitung eingetreten. |
| `device_added` | touchcue beginnt, einen FIDO-Schlüssel zu überwachen: sobald er eingesteckt wird, und beim Start des Daemons für jeden bereits vorhandenen Schlüssel. |
| `device_removed` | touchcue überwacht einen FIDO-Schlüssel nicht mehr, meist weil er abgezogen wurde. |
| `daemon_started` | Der Daemon wurde gestartet. |
| `daemon_stopping` | Der Daemon wird beendet. |

Eine einzelne Änderung kann mehrere Ereignisse auslösen. Diese treten in folgender Reihenfolge auf: `started`, `updated` oder `ended`, dann `revived`, `waiting` oder `lingering` und schließlich `touched`, `cancelled`, `failed` oder `timed_out`. Eine neue Anfrage löst also beispielsweise erst `started` und dann `waiting` aus.

`touched` wird ausgelöst, wenn die Anfrage endet. `cancelled`, `failed` und `timed_out` werden dagegen schon ausgelöst, sobald der Vorgang endet – zusammen mit `lingering` –, und von `ended` nicht wiederholt. Wird eine abgebrochene Anfrage reaktiviert, wartet sie wieder und kann später doch noch mit `touched` enden.

Geräteereignisse gibt es nur für FIDO-Schlüssel und nur, solange `[sources.fido]` aktiviert ist. Die `device_added`-Ereignisse für Schlüssel, die beim Start bereits vorhanden sind, folgen auf `daemon_started`. Verwirft der Kernel in seltenen Fällen Gerätebenachrichtigungen, löst touchcue für jeden Schlüssel erneut `device_removed` und `device_added` aus.

`match` legt nur fest, bei welchen Ereignissen ein Hook ausgeführt wird; ein Nachweis ist es nicht. Anwendungs- und Prozessnamen geben die Prozesse selbst an, sie sind keine authentifizierten Identitäten.

## Umgebung {#environment}

Befehle erben die Umgebung von touchcue, darunter `PATH`, `WAYLAND_DISPLAY` und `DBUS_SESSION_BUS_ADDRESS`. Hinzu kommen folgende Variablen:

- `TOUCHCUE_EVENT` enthält den Namen des Ereignisses, etwa `started`.
- Jeder [Platzhalter](./placeholders), den touchcue veröffentlicht, wird zu einer Variablen, deren Name aus `TOUCHCUE_` und dem Platzhalternamen in Großbuchstaben besteht, wobei Punkte durch Unterstriche ersetzt werden. Aus `app.name` wird zum Beispiel `TOUCHCUE_APP_NAME`, aus `request.state` wird `TOUCHCUE_REQUEST_STATE`.

Veröffentlicht werden dieselben Platzhalter wie auf dem [Ereignis-Socket](./configuration#ipc): alle `request.*`-Werte, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` und `requester.label`. Pfade ausführbarer Dateien, PIDs, UIDs und Befehlszeilen werden nie übergeben.

Unbekannte Werte werden nicht gesetzt. Steuerzeichen und unsichtbare Zeichen in einem Wert werden durch Leerzeichen ersetzt, mehrere aufeinanderfolgende Leerraumzeichen zu einem Leerzeichen zusammengefasst, und nach 1024 Zeichen wird der Wert abgeschnitten. Da ein Wert mit `-` beginnen kann, sollten Sie Programmen, die Optionen auswerten, davor `--` übergeben. Geräteereignisse setzen nur die `device.*`-Variablen, `daemon_started` und `daemon_stopping` nur `TOUCHCUE_EVENT`.

Variablen mit dem Präfix `TOUCHCUE_`, die touchcue selbst geerbt hat – etwa `TOUCHCUE_LOG` oder `TOUCHCUE_ASKPASS_FALLBACK` aus Ihrer Shell –, werden nicht an Hooks weitergegeben. Jede `TOUCHCUE_`-Variable, die ein Hook sieht, stammt also von touchcue.

## Ausführung

- **Keine Shell.** `command` enthält das Programm und seine Argumente und wird direkt ausgeführt. `$HOME`, `*`, `;` oder `|` werden so weitergegeben, wie sie dastehen. Das Programm wird im `PATH` gesucht.
- **Werte nur in Variablen.** Platzhalterwerte werden nie zu Argumenten; ein manipulierter Anwendungsname kann also keine Argumente einschleusen. Wenn Sie eine Shell brauchen, rufen Sie sie ausdrücklich auf und setzen Sie die Variablen in Anführungszeichen, wie im `notify-send`-Rezept weiter unten. Fügen Sie eine Variable nie direkt in den Skripttext ein.
- **Zeitlimit.** Läuft ein Befehl länger als `timeout_ms` (standardmäßig 5 Sekunden), erhält seine Prozessgruppe `SIGTERM`. Sobald sich der Befehl beendet, folgt `SIGKILL` an die Gruppe – spätestens aber eine Sekunde später, falls er sich nicht beendet. touchcue wartet in jedem Fall, bis der Befehl beendet ist.
- **Aufräumen.** Bei einem Zeitlimit oder beim Herunterfahren werden nur Prozesse beendet, die sich noch in der Prozessgruppe des Hooks befinden. Ein Prozess, der die Gruppe verlässt, etwa mit `setsid`, läuft weiter – ebenso Prozesse, die ein Befehl im Hintergrund gestartet hat, bevor er sich regulär beendet hat.
- **Ein- und Ausgabe.** stdin ist leer, außer bei [Hooks mit `until`](#standard-input). Die ersten 4 KiB von stdout und stderr werden ohne Steuerzeichen auf Debug-Ebene protokolliert (`touchcue run -vv`). Die Ausgabe von Hooks mit `until` wird nie protokolliert, da ein Launcher ausgeben kann, was Sie in ihn eintippen. Nachdem sich der Befehl beendet hat, liest touchcue seine Ausgabe noch höchstens 500 ms lang. Lässt ein Befehl Arbeit im Hintergrund weiterlaufen, sollte er deren Ausgabe daher umleiten, sonst geht sie verloren.
- **Fehler.** Kann ein Befehl nicht starten, endet er mit einem Status ungleich null, überschreitet er das Zeitlimit oder wird er beim Herunterfahren beendet, protokolliert touchcue eine Warnung mit dem Index des Hooks in der Konfiguration und dem Ereignis. Jeder Hook warnt höchstens einmal alle 10 Sekunden und zählt die unterdrückten Warnungen mit; diese werden auf Debug-Ebene protokolliert.
- **Reihenfolge und Grenzen.** Die Läufe eines Hooks starten in der Reihenfolge ihrer Ereignisse, höchstens `concurrency` gleichzeitig (standardmäßig 4). Auch wenn sie der Reihe nach starten, können sie in anderer Reihenfolge enden. Für Aktionen, die streng nacheinander ablaufen müssen, verwenden Sie daher `concurrency = 1`. Verschiedene Hooks laufen unabhängig voneinander. Jeder Hook puffert bis zu 256 Ereignisse. Ist seine Warteschlange voll, wird für diesen Hook das jeweils neueste Ereignis verworfen, die bereits wartenden bleiben erhalten, und touchcue protokolliert eine gedrosselte Warnung.
- **Herunterfahren.** Wird touchcue beendet, löst es `daemon_stopping` aus und wartet dann bis zu 2 Sekunden auf wartende und laufende Befehle, einschließlich der `daemon_stopping`-Befehle – unabhängig von deren `timeout_ms`. Befehle, die danach noch laufen, erhalten `SIGTERM` und `SIGKILL` wie bei einem Zeitlimit; wartende Läufe, die noch nicht gestartet sind, werden verworfen.

touchcue blockiert nie wegen eines Hooks: Popups, Benachrichtigungen und IPC arbeiten weiter, während Befehle laufen.

## Hooks für die Dauer einer Anfrage {#hooks-that-last-for-a-request}

Mit `until = "ended"` startet ein Hook für jede Anfrage einen Prozess, der bis zum Ende der Anfrage läuft. Das eignet sich für Programme, die die Aufforderung selbst anzeigen, etwa einen rofi-Dialog:

```toml
[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `until` | `ended` | keiner |
| `on_change` | `restart`, `stream`, `ignore` | `restart` |
| `stop_signal` | `SIGTERM`, `SIGINT`, `SIGHUP` | `SIGTERM` |
| `stop_grace_ms` | 0 bis 2000 | 1000 |
| `restart_interval_ms` | 0 bis 600000 | 200 |

`on`, `match` und `command` funktionieren wie bei anderen Hooks, allerdings mit diesen Einschränkungen:

- `on` darf nur `started` und `waiting` enthalten; jedes andere Ereignis ist ein Fehler.
- `timeout_ms` und `concurrency` sind nicht erlaubt: Der Prozess läuft so lange wie seine Anfrage, und über alle Hooks mit `until` hinweg laufen höchstens 8 Prozesse gleichzeitig. Ein weiterer Prozess startet erst, wenn ein anderer beendet ist, und es wird eine gedrosselte Warnung protokolliert.
- `on_change`, `stop_signal`, `stop_grace_ms` und `restart_interval_ms` sind ohne `until` nicht erlaubt.

### Wann der Prozess läuft

- Er startet, wenn eines der `on`-Ereignisse für eine Anfrage ausgelöst wird, deren Werte zu `match` passen – und zwar, sobald die Aufforderung der Anfrage angezeigt wird. „Angezeigt“ heißt hier: Keine Regel unterdrückt sie, egal was `[output]` tatsächlich darstellt. Ein Hook mit `until` funktioniert deshalb auch mit `mode = "none"`.
- Solange eine [Regel](./configuration#rules) die Aufforderung unterdrückt, wird der Prozess beendet, und für eine unterdrückte Anfrage wird keiner gestartet. Wird die Aufforderung wieder angezeigt, startet ein neuer Prozess.
- Er wird beendet, sobald die Anfrage endet.
- `waiting` wird auch ausgelöst, wenn ein erneuter Versuch des Clients eine Anfrage in der Nachlaufzeit reaktiviert. Ein Hook auf `waiting` startet einen geschlossenen Prozess also erneut.

`on_change` legt fest, was geschieht, wenn sich ein Wert der Anfrage ändert:

- `restart` beendet den Prozess und startet einen neuen, sobald der alte beendet ist, höchstens einmal pro `restart_interval_ms`. Änderungen in der Zwischenzeit werden zusammengefasst, sodass die jeweils neuesten Werte gelten. Ändert sich nur `request.elapsed`, wird nicht neu gestartet.
- `stream` lässt den Prozess weiterlaufen und schreibt jede Änderung auf seine stdin – auch dann, wenn sich nur `request.elapsed` geändert hat.
- `ignore` lässt den Prozess weiterlaufen und teilt ihm nichts mit.

Beendet sich ein Prozess, bevor seine Anfrage endet – mit beliebigem Exit-Status –, oder kann er gar nicht erst starten, gilt er als geschlossen, etwa ein Menü, das mit Escape geschlossen wurde. touchcue startet ihn erst wieder, wenn sich ein Wert der Anfrage ändert, bzw. bei `on_change = "ignore"` erst, wenn eines der `on`-Ereignisse erneut ausgelöst wird. Eine Änderung, die vor dem Beenden eingetroffen ist, wird berücksichtigt: Beendet sich bei `restart` ein Prozess, während ein Neustart noch auf `restart_interval_ms` wartet, wird er mit den neuesten Werten erneut gestartet. Beendet sich der Prozess von selbst, schickt touchcue außerdem `SIGKILL` an seine Prozessgruppe. Programme, die er im Hintergrund zurückgelassen hat, enden so mit ihm und unterlaufen nicht die Grenze von 8 Prozessen.

### Umgebung eines Prozesses

Ein Prozess erhält dieselbe [Umgebung](#environment) wie andere Hooks, mit folgenden Unterschieden:

- `TOUCHCUE_EVENT` ist `show` für den ersten Prozess einer Anfrage und `update` für einen Prozess, der nach einer Änderung oder nach einer Unterdrückung der Aufforderung erneut gestartet wurde.
- `TOUCHCUE_TITLE` und `TOUCHCUE_BODY` enthalten Titel und Text der Aufforderung genau so, wie die Benachrichtigung sie nach Anwendung der Regeln zeigt – mit derselben Bereinigung und Längenbegrenzung wie die übrigen Werte. Befindet sich eine Anfrage nach einem Abbruch oder einer Zeitüberschreitung in der Nachlaufzeit, endet der Text wie in der Benachrichtigung mit `(cancelled)` bzw. `(timed out)` in der [Sprache](./configuration#locale) der Aufforderung. Für Skripte, die das Ergebnis selbst formulieren, enthält `TOUCHCUE_REQUEST_STATE` den Zustand. Diese Werte entstehen aus den konfigurierten Vorlagen und können daher auch nicht veröffentlichte Platzhalter enthalten, einschließlich Befehlszeilen.

Andere Hooks erhalten weder `TOUCHCUE_TITLE` noch `TOUCHCUE_BODY`, da sie auch für unterdrückte und beendete Anfragen laufen, die keine Aufforderung haben.

### Standardeingabe {#standard-input}

Die stdin eines Prozesses ist eine Pipe, über die pro Zeile ein JSON-Objekt kommt:

```json
{"op":"show","id":3,"title":"Touch Yubico","body":"Firefox is waiting for a passkey","values":{"app.name":"Firefox","request.method":"fido2","request.state":"waiting"}}
{"op":"update","id":3,"title":"Touch Yubico","body":"Firefox is waiting for a passkey (cancelled)","values":{"app.name":"Firefox","request.method":"fido2","request.state":"cancelled"}}
{"op":"hide","id":3}
```

- `op` ist in der ersten Zeile `show` oder `update`, genau wie `TOUCHCUE_EVENT`. Mit `on_change = "stream"` folgt für jede spätere Änderung eine `update`-Zeile; ist die Anfrage beendet oder wird ihre Aufforderung unterdrückt, kommt `hide`.
- `id` ist die ID der Anfrage, eine Zahl.
- `title` und `body` enthalten dieselben Zeichenketten wie `TOUCHCUE_TITLE` und `TOUCHCUE_BODY`, während der Nachlaufzeit also einschließlich des Ergebnisses; `request.state` in `values` gibt das Ergebnis für sich allein an.
- `values` enthält die veröffentlichten Platzhalter nach Namen, bereinigt wie in der Umgebung. Das Beispiel zeigt nur einen Teil davon.

touchcue wartet nie darauf, dass ein Prozess liest. Jeder Prozess hat eine Warteschlange für 64 Zeilen. Ist sie voll, wird die älteste wartende `update`-Zeile verworfen und eine gedrosselte Warnung protokolliert; `show`- und `hide`-Zeilen bleiben immer erhalten. Schließt ein Prozess stdin oder beendet er sich, gehen nur seine eigenen Zeilen verloren. Ein `stream`-Prozess, der nichts liest, während 256 Aktualisierungen verworfen werden, wird beendet, und es wird eine gedrosselte Warnung protokolliert. Hat sich seit der letzten Aktualisierung, die er erhalten hat, ein anderer Wert als `request.elapsed` geändert, wird er mit den neuesten Werten neu gestartet, höchstens einmal pro `restart_interval_ms`; andernfalls gilt er als geschlossen.

### Beenden eines Prozesses

Um einen Prozess zu beenden, schreibt touchcue die noch ausstehenden Zeilen und die `hide`-Zeile, schließt stdin und schickt dann `stop_signal` an die Prozessgruppe. Für das Schreiben bleiben höchstens 200 ms. Liest der Prozess in dieser Zeit nicht, erhält er das Signal trotzdem und sieht vor dem Ende seiner Eingabe womöglich eine abgeschnittene letzte Zeile. Das Signal folgt unmittelbar auf das Schließen von stdin. Muss ein Programm noch auf die `hide`-Zeile oder das Ende seiner Eingabe reagieren, muss es dieses Signal daher selbst behandeln, zum Beispiel ignorieren. Hat sich der Prozess nach `stop_grace_ms` noch nicht beendet, folgt `SIGKILL` an die Gruppe; touchcue wartet in jedem Fall auf ihn.

Ein Prozess, der für einen Neustart beendet wird, erhält keine `hide`-Zeile.

Wird touchcue beendet, schreibt es keine weiteren Zeilen mehr; die Prozesse erhalten sofort `stop_signal` und nach `stop_grace_ms` `SIGKILL`. Das passt in die 4 Sekunden, die der Daemon beim Herunterfahren allen Hooks zusammen einräumt; die systemd-Unit gibt dem gesamten Daemon 10 Sekunden zum Beenden.

## Rezepte

### Beim Start einer Anfrage einen Ton abspielen

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

Ohne PipeWire verwenden Sie `paplay` mit derselben Datei. Soll der Ton nur bei bestimmten Anfragen erklingen, ergänzen Sie `match`:

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "openpgp" }
command = ["paplay", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

### Benachrichtigen, wenn der Schlüssel berührt wurde

`notify-send` erwartet den Text als Argumente; deshalb liest eine Shell ihn aus der Umgebung:

```toml
[[hooks]]
on = ["touched"]
command = ["sh", "-c", 'notify-send -- "Security key touched" "${TOUCHCUE_APP_NAME:-An application}"']
```

Durch die einfachen Anführungszeichen ist die TOML-Zeichenkette ein Literal, sodass das Skript unverändert bei `sh` ankommt. Die doppelten Anführungszeichen sorgen dafür, dass der Anwendungsname ein einzelnes Argument bleibt, und `--` verhindert, dass `notify-send` einen Namen, der mit `-` beginnt, als Option interpretiert.

### Ein waybar-Modul aktualisieren

Ein waybar-Modul vom Typ `custom` mit `"signal": 8` führt sein `exec` erneut aus, wenn waybar `SIGRTMIN+8` empfängt:

```toml
[[hooks]]
on = ["started", "ended"]
command = ["pkill", "-RTMIN+8", "waybar"]
concurrency = 1
```

`concurrency = 1` sorgt dafür, dass die Signale nacheinander in der Reihenfolge der Ereignisse gesendet werden. So folgt die letzte Aktualisierung des Moduls auf die letzte Änderung.

### Eine Hyprland-Benachrichtigung anzeigen

```toml
[[hooks]]
on = ["waiting"]
command = ["hyprctl", "notify", "1", "5000", "rgb(ff1ea3)", "Touch your security key"]
```

Die Argumente sind das Symbol (`1` steht für Info), die Dauer in Millisekunden, die Farbe und die Nachricht.
