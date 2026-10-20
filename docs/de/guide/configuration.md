# Konfiguration

touchcue liest `$XDG_CONFIG_HOME/touchcue/config.toml`, ersatzweise `~/.config/touchcue/config.toml`. Mit `--config <PATH>` geben Sie eine andere Datei an; diese muss dann auch existieren. Fehlt die Standarddatei, gelten die unten aufgeführten Standardwerte.

Alle Schlüssel sind optional. Unbekannte Schlüssel und Werte außerhalb des zulässigen Bereichs gelten als Fehler: `touchcue run` startet dann nicht, und `touchcue check` meldet sie – jeweils mit der Stelle in der Datei, sofern sie bekannt ist.

## Beispiel

Die Standardwerte, vollständig ausgeschrieben:

```toml
# locale is read from the environment when unset

[output]
mode = "popup"
fallback = "notification"

[popup]
position = "center"
output = "focused"
min_display_ms = 800
show_delay_ms = 0
modal = false
modal_dim = 0.4
modal_dismiss = true
animations = true

[notification]
urgency = "critical"
safety_timeout_s = 60

[icons]
# theme is detected when unset

[templates]
title = 'Touch {{ device.vendor or "your security key" }}'
body = '{{ requester.label or process.name or "An application" }} is waiting for {{ request.action }}'

[sources.fido]
enabled = true
keepalive_timeout_ms = 1500
retry_window_ms = 1000

[sources.gpg]
enabled = true

[ipc]
enabled = true

[dbus]
enabled = true

[compat.maxbaz_socket]
enabled = false

[requester]
skip = [
  "sh", "bash", "dash", "zsh", "fish", "nu", "ksh", "mksh", "tcsh", "csh", "elvish", "xonsh",
  "tmux*", "screen", "zellij", "herdr", "abduco", "dtach",
  "timeout", "nice", "nohup", "setsid", "stdbuf", "time", "xargs", "flock", "ionice", "chrt",
  "taskset", "env", "sudo", "doas", "su", "run0",
  "systemd", "init",
]
extend_skip = []
```

## `locale` {#locale}

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `locale` | ein Locale-Name wie `pl`, `de` oder `pl_PL.UTF-8` | aus der Umgebung |

`locale` legt die Sprache der eingebauten Texte der Aufforderung fest: des Standardtitels und -texts, von [`request.action`](./placeholders#values) sowie der Zusätze `(cancelled)` und `(timed out)`. Verfügbar sind Englisch, Polnisch (`pl`) und Deutsch (`de`). Ist `locale` nicht gesetzt oder leer, liest `touchcue run` beim Start einmalig die erste nicht leere der Variablen `LC_ALL`, `LC_MESSAGES` und `LANG`. Die Sprache ist der Teil des Namens vor dem ersten `_`, `.` oder `@`; `C`, `POSIX` und alle anderen Sprachen bedeuten Englisch. Es entscheidet der erste nicht leere Wert, ein unbekanntes `locale` fällt also nicht auf die Umgebung zurück. Selbst geschriebene Vorlagen werden nicht übersetzt. Als Schlüssel der obersten Ebene steht `locale` in der Datei vor dem ersten Abschnitt `[section]`:

```toml
locale = "pl"

[output]
mode = "popup"
```

## `[output]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `mode` | `popup`, `notification`, `both`, `none`, `command` | `popup` |
| `fallback` | dieselben | `notification` |

touchcue verwendet `mode`, sofern der Desktop diese Art der Anzeige unterstützt, andernfalls `fallback`. Ist beides nicht möglich, wird nichts angezeigt.

- `popup` blendet ein Popup über allen Fenstern ein, das keinen Eingabefokus an sich zieht. Ist `WAYLAND_DISPLAY` oder `WAYLAND_SOCKET` gesetzt und unterstützt der Compositor es, kommt Wayland layer-shell zum Einsatz, andernfalls X11, sofern `DISPLAY` gesetzt ist – das schließt XWayland ein. Reißt die Verbindung zum Display ab, etwa weil der Compositor neu gestartet wurde, verbindet sich touchcue auf demselben Weg neu wie beim Start: über `WAYLAND_DISPLAY`, ersatzweise über `DISPLAY`. Der erste Versuch erfolgt nach 1 Sekunde, danach immer seltener, bis hin zu einmal pro Minute. Noch sichtbare Aufforderungen werden anschließend erneut angezeigt. Kehrt der Compositor unter einem anderen Socket-Namen zurück, folgt touchcue ihm nicht und verbindet sich unter Umständen stattdessen über `DISPLAY` mit XWayland.
- `notification` verschickt eine Desktop-Benachrichtigung über D-Bus; gezeichnet wird sie vom Benachrichtigungsserver, hier mako:

  ![Eine mako-Benachrichtigung: „Touch your security key“, „Browser is waiting for a passkey“](/screenshots/notification.png)
- `both` zeigt das Popup und zusätzlich eine Benachrichtigung.
- `none` zeigt nichts an.
- `command` wird zwar akzeptiert, aber mit einer Warnung übergangen. Um Aufforderungen über Programme wie rofi oder fuzzel anzuzeigen, verwenden Sie [Hooks mit `until`](./hooks#hooks-that-last-for-a-request); siehe [Launcher](./launchers).

## `[popup]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `position` | `center`, `top-left`, `top-right`, `bottom-left`, `bottom-right`, `top`, `bottom` | `center` |
| `output` | `"focused"`, `"all"`, `"cursor"` oder eine Liste von Bildschirmnamen | `"focused"` |
| `min_display_ms` | 0 bis 600000 | 800 |
| `show_delay_ms` | 0 bis 600000 | 0 |
| `modal` | `true`, `false` | `false` |
| `modal_dim` | 0.0 bis 1.0 | 0.4 |
| `modal_dismiss` | `true`, `false` | `true` |
| `animations` | `true`, `false` | `true` |

![Ein Popup in der oberen rechten Ecke eines Bildschirms, mit position = "top-right"](/screenshots/corner.png)

Popups werden in der Skalierung des Bildschirms gezeichnet, auf dem sie erscheinen. Unterstützt der Wayland-Compositor fractional-scale-v1, sind auch gebrochene Faktoren wie 1,5 möglich, andernfalls gilt der ganzzahlige Faktor, den der Compositor bevorzugt. Unter X11 ergibt sich die Skalierung aus `Xft.dpi`.

`min_display_ms` legt fest, wie lange ein Popup mindestens sichtbar bleibt, nachdem es erschienen ist. `show_delay_ms` legt fest, wie lange eine Anfrage warten muss, bevor ihr Popup erscheint.

![Das Standard-Popup, zentriert: „Touch your security key“, „Browser is waiting for a passkey“](/screenshots/default.png)

Mehrere Popups auf einem Bildschirm werden in der Reihenfolge gestapelt, in der ihre Anfragen begonnen haben: vom Bildschirmrand weg oder, bei `center`, senkrecht um die Mitte herum.

![Zwei gestapelte Popups, eines vom Browser und eines von claude in einem Terminal](/screenshots/stacked.png)

### Zustandsanzeige {#state-overlay}

Das Anwendungssymbol wird abgedunkelt dargestellt; mittig darüber erscheint der Zustand der Anfrage. Hat ein Popup kein Symbol, steht die Zustandsanzeige allein an dessen Stelle.

- Solange die Anfrage auf eine Berührung wartet, dreht sich eine Ladeanimation über dem Symbol.
- Endet der Vorgang, wird die Ladeanimation durch ein Zeichen auf einem farbigen Kreis ersetzt: ein Häkchen auf Grün nach einer Berührung, ein Kreuz auf Rot nach einem Abbruch oder Fehler und eine Uhr auf Bernstein nach einer Zeitüberschreitung.

  ![Ein Popup, bei dem sich eine Ladeanimation über dem abgedunkelten Browsersymbol dreht und nach einer Berührung einem Häkchen auf grünem Kreis weicht](/screenshots/anim-touched.gif)

  ![Ein Popup, bei dem die Ladeanimation nach einem Abbruch einem Kreuz auf rotem Kreis weicht](/screenshots/anim-cancelled.gif)

  ![Ein Popup, bei dem die Ladeanimation nach einer Zeitüberschreitung einer Uhr auf bernsteinfarbenem Kreis weicht](/screenshots/anim-timed-out.gif)
- Versucht der Client es erneut, erscheint statt des Zeichens wieder die Ladeanimation.

  ![Ein Popup, bei dem die Ladeanimation der Uhr für die Zeitüberschreitung weicht, sich beim erneuten Versuch des Clients wieder dreht und schließlich dem Häkchen für die Berührung weicht](/screenshots/anim-retry.gif)

Jeder Wechsel wird über 250 ms eingeblendet und wächst dabei auf seine endgültige Größe. Mit `animations = false` oder wenn der Desktop reduzierte Bewegung anfordert, steht die Ladeanimation still, und jedes Zeichen erscheint sofort. touchcue liest diese Desktop-Einstellung einmalig beim Start über das XDG-Desktop-Portal: `reduced-motion` = 1 in `org.freedesktop.appearance` oder `enable-animations` = false in `org.gnome.desktop.interface`, sofern das Portal diesen Wert bereitstellt. Ohne Portal werden Bewegungen nicht reduziert.

### Bildschirme

`output` legt fest, auf welchen Monitoren das Popup erscheint. Der Bildschirm wird in dem Moment gewählt, in dem das Popup erscheint.

- `"focused"` überlässt die Wahl dem Wayland-Compositor, der in der Regel den fokussierten Monitor nimmt. Unter X11 ist es der Monitor unter dem Mauszeiger.
- `"all"` zeigt auf jedem Bildschirm ein Popup.

  ![Dasselbe Popup zentriert auf jedem von zwei Bildschirmen](/screenshots/all-outputs.png)
- `"cursor"` verwendet den Bildschirm unter dem Mauszeiger. Unter Wayland klappt das nur mit Hyprland, über dessen IPC-Socket. In allen anderen Fällen oder wenn die Abfrage fehlschlägt, protokolliert touchcue eine Warnung und weicht auf `"focused"` aus. Unter X11 entspricht der Wert `"focused"`.
- Eine Liste wie `["DP-1", "HDMI-A-1"]` zeigt auf jedem aufgeführten Bildschirm, der tatsächlich vorhanden ist, ein Popup. Unter Wayland stammen die Namen vom Compositor (`hyprctl monitors`, `swaymsg -t get_outputs`), unter X11 von RandR (`xrandr --listmonitors`). Ist keiner davon vorhanden, protokolliert touchcue eine Warnung und weicht auf `"focused"` aus. Eine leere Liste oder ein leerer Name ist ein Fehler.

### Modale Popups

Mit `modal = true` fängt ein Popup, das auf eine Berührung wartet, zusätzlich Klicks ab: Auf jedem seiner Bildschirme liegt hinter dem Popup eine bildschirmfüllende Ebene, die alle Klicks abfängt. Die Tastatur wird dabei nie gesperrt; Tastatureingaben gehen weiterhin an das fokussierte Fenster. Die Ebene verschwindet, sobald auf diesem Bildschirm keine Anfrage mehr wartet, spätestens aber nach 120 Sekunden – auch wenn dann noch Anfragen warten. Anfragen, die beginnen, während die Ebene angezeigt wird, verlängern diese Zeit nicht. Nach Ablauf der 120 Sekunden erhält der Bildschirm 30 Sekunden lang keine neue Ebene.

![Ein modales Popup über einem abgedunkelten Bildschirm](/screenshots/modal-dim.png)

- `modal_dim` legt fest, wie stark die Ebene abdunkelt, bis hin zu `1.0` (schwarz). Mit `0.0` werden Klicks abgefangen, ohne dass etwas abgedunkelt wird:

  ![Ein modales Popup mit modal_dim = 0.0: Der Bildschirm ist nicht abgedunkelt](/screenshots/modal-nodim.png)
- Mit `modal_dismiss = true` blendet ein Klick oder Tippen auf die Ebene die Popups und Ebenen der dort angezeigten Anfragen aus. Ausgeblendet wird nur die Anzeige; die Anfrage selbst wartet weiter auf eine Berührung. Die nächste Anfrage auf diesem Bildschirm erhält sofort wieder eine neue Ebene. Ausblenden lässt sich nur mit der linken, mittleren und rechten Maustaste, nicht durch Scrollen oder mit den Seitentasten.
- Mit `modal_dismiss = false` können aufeinanderfolgende Anfragen einen Bildschirm länger als 120 Sekunden abgedunkelt halten: Die Grenze gilt für jede Ebene einzeln, und eine neue Anfrage erhält eine neue Ebene, sobald die vorherige verschwunden ist oder 30 Sekunden, nachdem diese die Grenze erreicht hat. Die Tastatur bleibt die ganze Zeit benutzbar.

Unter X11 fängt die Ebene Klicks ab, ohne abzudunkeln; `modal_dim` hat dort keine Wirkung. Unter Wayland liegt die Ebene auf der overlay-Ebene von layer-shell, also über Fenstern und Leisten.

## `[notification]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `urgency` | `low`, `normal`, `critical` | `critical` |
| `safety_timeout_s` | 0 bis 600 | 60 |

`safety_timeout_s` ist die Zeit, nach der der Benachrichtigungsserver eine Benachrichtigung zurückzieht, selbst wenn die Anfrage noch wartet. touchcue übergibt den Wert als Ablaufzeit der Benachrichtigung; `0` bedeutet „nie“.

## `[icons]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `theme` | der Verzeichnisname eines Symbolthemas, etwa `Papirus-Dark` | automatisch erkannt |

Anwendungssymbole aus Desktop-Einträgen und die Symbolnamen aus [`[[rules]]`](#rules) werden in der Größe 64 Pixel gesucht: zuerst in diesem Thema, dann in den Themen, von denen es erbt, und schließlich in `hicolor`, so wie es die [freedesktop Icon Theme Specification](https://specifications.freedesktop.org/icon-theme/latest/) beschreibt. Nach Themen wird in dieser Reihenfolge gesucht: in `~/.icons`, in `icons` unterhalb von `$XDG_DATA_HOME` (ohne gesetzte Variable `~/.local/share`) und unterhalb jedes Verzeichnisses aus `$XDG_DATA_DIRS` (ohne gesetzte Variable `/usr/local/share:/usr/share`) sowie in `/usr/share/pixmaps`. Maßgeblich für ein Thema ist die erste gefundene `index.theme`. Ist ein Symbol in keinem Thema enthalten, wird anschließend direkt in diesen Verzeichnissen nach einer gleichnamigen Datei gesucht. Berücksichtigt werden nur PNG- und SVG-Dateien; eine Endung `.png`, `.svg` oder `.xpm` am Symbolnamen, wie bei `Icon=app.png` in manchen Desktop-Einträgen, wird ignoriert. Das Ergebnis jeder Suche wird zwischengespeichert: Ein Name, der in keinem Thema gefunden wurde, wird erst wieder gesucht, wenn der Daemon neu startet oder mehr als 512 Namen nachgeschlagen wurden. Bei einer gefundenen Datei wird vor jeder Verwendung geprüft, ob sie noch existiert. Ein Wert, der leer ist, `.` oder `..` lautet oder `/` bzw. Steuerzeichen enthält, ist ein Konfigurationsfehler.

Ist `theme` nicht gesetzt, ermittelt `touchcue run` das Thema einmalig beim Start, und zwar aus der ersten der folgenden Quellen, die ein installiertes Thema nennt:

1. die Einstellungen des XDG-Desktop-Portals: `icon-theme` in `org.gnome.desktop.interface`, danach `Theme` in `org.kde.kdeglobals.Icons`;
2. `gtk-icon-theme-name` in der Gruppe `[Settings]` von `~/.config/gtk-4.0/settings.ini`, danach von `~/.config/gtk-3.0/settings.ini`;
3. `Theme` in der Gruppe `[Icons]` von `~/.config/kdeglobals`;
4. `icon_theme` in der Gruppe `[Appearance]` von `~/.config/qt6ct/qt6ct.conf`, danach von `~/.config/qt5ct/qt5ct.conf`;
5. `hicolor`.

`~/.config` steht für `$XDG_CONFIG_HOME`, sofern die Variable gesetzt ist. Enthält `XDG_CURRENT_DESKTOP` den Eintrag `KDE`, wird die KDE-Einstellung des Portals vor der GNOME-Einstellung gelesen. Das Portal wird übersprungen, wenn auf dem Sitzungsbus kein Portal installiert ist oder läuft, und auch dann, wenn Verbindungsaufbau, Start und das Lesen beider Einstellungen zusammen länger als 1,5 s dauern. Die gesamte Erkennung einschließlich des Einlesens des gewählten Themas dauert höchstens 3 s; danach startet der Daemon mit `hicolor` und protokolliert eine Warnung. Ist ein konfiguriertes oder erkanntes Thema nicht installiert, wird es mit einer Warnung übersprungen und die nächste Quelle probiert; auf ein konfiguriertes Thema folgt dabei die oben beschriebene Erkennung. Wird das Thema auf dem Desktop geändert, übernimmt der Daemon es erst nach einem Neustart. `touchcue check` gibt das Thema samt Herkunft aus, zum Beispiel `icons: theme = "Papirus-Dark" (portal org.gnome.desktop.interface)`.

## `[templates]`

::: v-pre
| Schlüssel | Standard |
| --- | --- |
| `title` | `Touch {{ device.vendor or "your security key" }}` |
| `body` | `{{ requester.label or process.name or "An application" }} is waiting for {{ request.action }}` |
:::

Dies sind die englischen Standardwerte. Ist als [`locale`](#locale) Polnisch oder Deutsch eingestellt, lauten sie:

::: v-pre
| Sprache | `title` | `body` |
| --- | --- | --- |
| Polnisch | `Dotknij klucza {{ device.vendor or "bezpieczeństwa" }}` | `{{ requester.label or process.name or "Aplikacja" }} czeka na {{ request.action }}` |
| Deutsch | `Bitte {{ device.vendor ~ "-Schlüssel" if device.vendor else "Sicherheitsschlüssel" }} berühren` | `{{ requester.label or process.name or "Eine Anwendung" }} wartet auf {{ request.action }}` |
:::

Der Standardtext nennt über [`requester.label`](./placeholders#requester) den Urheber und seine Anwendung, etwa `claude in Kitty is waiting for a GPG signature` – oder `Firefox is waiting for a passkey`, wenn die Anwendung selbst anfragt. [`request.action`](./placeholders#values) beschreibt in Worten, worauf die Anfrage wartet.

Vorlagen verwenden Jinja-Syntax mit [Platzhaltern](./placeholders#template-syntax). Schlägt das Rendern einer Vorlage fehl, tritt an ihre Stelle die nächste in dieser Reihenfolge: die Vorlage der passenden Regel, die Vorlage aus `[templates]`, der oben genannte Standard und schließlich ein leerer Text. Endet eine Anfrage ohne Berührung, bleibt ihre Aufforderung noch kurz stehen: Das Popup zeigt das Ergebnis in seiner [Zustandsanzeige](#state-overlay), eine Benachrichtigung hängt `(cancelled)` oder `(timed out)` an den Text an, auf Polnisch `(anulowano)` oder `(upłynął limit czasu)`, auf Deutsch `(abgebrochen)` oder `(Zeitüberschreitung)`.

Der Titel wird auf höchstens zwei Zeilen umbrochen, der Text auf höchstens sechs; was darüber hinausgeht, wird mit Auslassungspunkten abgeschnitten:

![Ein Popup, dessen langer Titel und Text umbrochen werden und mit Auslassungspunkten enden](/screenshots/long-text.png)

![Drei Popups mit demselben Text: eine berührte Anfrage mit Häkchen, eine abgebrochene mit Kreuz und eine abgelaufene mit Uhr](/screenshots/outcomes.png)

## `[[rules]]`

Regeln passen die Aufforderung für bestimmte Anfragen an. Es gilt die erste Regel, bei der alle `match`-Einträge mit den Platzhalterwerten der Anfrage übereinstimmen; alle weiteren Regeln werden ignoriert.

```toml
[[rules]]
match = { "process.name" = "ssh", "request.method" = "fido2" }
title = "Touch your key for ssh"
body = '{{ requester.label or "ssh" }} is signing in{% if request.detail %}: {{ request.detail }}{% endif %}'
icon = "utilities-terminal"

[[rules]]
match = { "requester.name" = "claude" }
icon = "~/.local/share/icons/claude.svg"

[[rules]]
match = { "app.id" = "org.mozilla.firefox" }
suppress = true
```

| Schlüssel | Typ | Bedeutung |
| --- | --- | --- |
| `match` | Tabelle mit Zeichenketten | Platzhalternamen und ihre Werte. Pflichtangabe, darf nicht leer sein. Werte werden exakt und unter Beachtung der Groß- und Kleinschreibung verglichen; ein Platzhalter ohne Wert stimmt nie überein. |
| `title` | Vorlage | Ersetzt `templates.title`. |
| `body` | Vorlage | Ersetzt `templates.body`. |
| `icon` | Pfad oder Name | Symbol, das anstelle des Anwendungssymbols erscheint: ein absoluter Pfad, ein mit `~/` beginnender Pfad in Ihrem Home-Verzeichnis oder ein Symbolname. |
| `suppress` | Boolescher Wert | Unterdrückt jede Anzeige für passende Anfragen. Über IPC werden sie trotzdem veröffentlicht. Standard `false`. |

Ein `icon`-Pfad muss auf eine PNG- oder SVG-Datei verweisen. Ein `~/`-Pfad darf das Home-Verzeichnis nicht verlassen: Ein absoluter Rest wie in `~//etc` oder ein `..`-Bestandteil ist ein Konfigurationsfehler. Ein Wert ohne `/`, etwa `utilities-terminal`, gilt als Symbolname und wird wie Anwendungssymbole im [Symbolthema](#icons) gesucht. Alle übrigen relativen Pfade sind ein Konfigurationsfehler. Symbolpfade werden jedes Mal aufgelöst, wenn eine Aufforderung erscheint oder sich ändert, und zwar innerhalb von 0,5 s. Dateien, die erst nach dem Start angelegt wurden, werden dabei ebenfalls berücksichtigt. Ein nicht gefundener Symbolname gilt so lange als nicht vorhanden, bis der Daemon neu startet oder mehr als 512 Namen nachgeschlagen wurden, wie unter [`[icons]`](#icons) beschrieben. Wird nicht rechtzeitig eine Datei gefunden, zeigt die Aufforderung das Anwendungssymbol, und der Daemon protokolliert eine Warnung.

![Ein Popup mit dem Titel und dem Schlüsselsymbol einer Regel unter einem Popup mit dem Anwendungssymbol](/screenshots/rule-icon.png)

Das angezeigte Symbol ist kein Beweis dafür, welches Programm die Berührung angefordert hat: Jedes Programm, das unter Ihrem Benutzerkonto läuft, kann täuschend ähnliche Symbole in `~/.icons` oder `~/.local/share/icons` ablegen oder Desktop-Einträge installieren, die das Symbol einer anderen Anwendung verwenden.

## `[[hooks]]`

Hooks führen bei touchcue-Ereignissen einen Befehl aus. Welche Ereignisse es gibt, welche Umgebung der Befehl erhält und einige Rezepte finden Sie unter [Hooks](./hooks). Mit Hooks führt diese Datei Befehle unter Ihrem Benutzerkonto aus. Sorgen Sie deshalb dafür, dass nur Sie sie bearbeiten können, und starten Sie den Daemon nie als root mit einer Konfigurationsdatei, die ein anderer Benutzer beschreiben kann.

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "fido2" }
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
timeout_ms = 5000
concurrency = 4
```

| Schlüssel | Typ | Bedeutung |
| --- | --- | --- |
| `on` | Liste von Ereignisnamen | Ereignisse, bei denen der Befehl ausgeführt wird. Pflichtangabe, darf nicht leer sein. Ein unbekannter Name ist ein Fehler. |
| `match` | Tabelle mit Zeichenketten | Platzhalternamen und Werte, die alle übereinstimmen müssen, wie bei `[[rules]]`. Optional; ohne `match` führt jedes aufgeführte Ereignis den Befehl aus. Geräteereignisse haben nur `device.*`-Werte, Daemon-Ereignisse gar keine. Verlangt `match` Werte, die das Ereignis nicht hat, wird der Befehl nicht ausgeführt. `match` dient nur der Auswahl von Ereignissen: Anwendungs- und Prozessnamen legen die Prozesse selbst fest. |
| `command` | Liste von Zeichenketten | Das Programm und seine Argumente; es wird ohne Shell ausgeführt. Pflichtangabe; das Programm darf nicht leer sein. |
| `timeout_ms` | 1 bis 600000 | Wie lange der Befehl laufen darf, bevor er beendet wird. Standard 5000. Nicht zusammen mit `until` erlaubt. |
| `concurrency` | 1 bis 32 | Wie viele Läufe dieses Hooks höchstens gleichzeitig stattfinden. Standard 4. Nicht zusammen mit `until` erlaubt. |
| `until` | `ended` | Startet pro Anfrage einen Prozess, der bis zum Ende der Anfrage läuft, statt eines Laufs pro Ereignis; `on` darf dann nur `started` und `waiting` enthalten. Siehe [Hooks für die Dauer einer Anfrage](./hooks#hooks-that-last-for-a-request). |
| `on_change` | `restart`, `stream`, `ignore` | Nur mit `until`: was mit dem Prozess geschieht, wenn sich die Anfrage ändert. Standard `restart`. |
| `stop_signal` | `SIGTERM`, `SIGINT`, `SIGHUP` | Nur mit `until`: Signal, mit dem der Prozess beendet wird. Standard `SIGTERM`. |
| `stop_grace_ms` | 0 bis 2000 | Nur mit `until`: Zeit zwischen `stop_signal` und `SIGKILL`. Standard 1000. |
| `restart_interval_ms` | 0 bis 600000 | Nur mit `until`: Mindestabstand zwischen zwei Starts des Prozesses einer Anfrage. Standard 200. |

## `[sources.fido]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `enabled` | Boolescher Wert | `true` |
| `keepalive_timeout_ms` | 100 bis 600000 | 1500 |
| `retry_window_ms` | 1 bis 600000 | 1000 |

`keepalive_timeout_ms` legt fest, wie lange eine FIDO-Anfrage ohne Keepalive vom Schlüssel bestehen bleibt. `retry_window_ms` legt fest, wie lange ein beendeter Vorgang auf einen erneuten Versuch des Clients wartet. Versucht der Client es in dieser Zeit erneut, wird dieselbe Aufforderung fortgesetzt.

## `[sources.gpg]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `enabled` | Boolescher Wert | `true` |

Aktiviert den Helfer-Socket `$XDG_RUNTIME_DIR/touchcue/helper.sock`, über den [der gpg-agent-Wrapper](./gpg) und [`touchcue askpass`](./ssh-askpass) Vorgänge melden.

## `[requester]`

| Schlüssel | Werte | Standard |
| --- | --- | --- |
| `skip` | Liste von Prozessnamen | Shells, Multiplexer, Wrapper und Dienstmanager, wie im Beispiel oben |
| `extend_skip` | Liste von Prozessnamen | `[]` |

Diese Listen legen fest, welche Prozesse bei der Suche nach dem [Urheber](./placeholders#requester) übersprungen werden. `skip` ersetzt die eingebaute Liste; mit `skip = []` wird nichts übersprungen. `extend_skip` ergänzt `skip` bzw. die eingebaute Liste um weitere Namen. So überspringen Sie zusätzlich den Editor, in dessen Terminal Sie arbeiten:

```toml
[requester]
extend_skip = ["nvim"]
```

Ein Eintrag ist entweder ein Name oder ein Präfix mit abschließendem `*`, etwa `tmux*`. Ein Name wird exakt und unter Beachtung der Groß- und Kleinschreibung mit dem `comm` des Prozesses verglichen oder, falls sich `comm` nicht lesen lässt, mit dem Dateinamen seiner ausführbaren Datei. Der Kernel kürzt `comm` auf 15 Byte; für längere Namen verwenden Sie daher einen Präfix-Eintrag. Ungültige Einträge werden mit ihrer Stelle in der Datei gemeldet. `touchcue check` gibt die tatsächlich wirksame Liste aus. Wenn Sie davon ausgehen möchten, kopieren Sie die Liste hinter `skip =` in Ihre Konfiguration.

## IPC

| Abschnitt | Schlüssel | Standard |
| --- | --- | --- |
| `[ipc]` | `enabled` | `true` |
| `[dbus]` | `enabled` | `true` |
| `[compat.maxbaz_socket]` | `enabled` | `false` |

Alle Endpunkte setzen `XDG_RUNTIME_DIR` voraus. Sie senden ausschließlich; Clients können darüber nichts verändern.

- **Ereignis-Socket**, `$XDG_RUNTIME_DIR/touchcue/events.sock`, gesteuert über `[ipc]`. Jede Zeile ist ein JSON-Objekt mit `kind` (`started`, `updated` oder `ended`), `id`, `state`, `reason` (nur bei `ended` gesetzt: `touched`, `cancelled`, `failed` oder `timed_out`), `source` und `values`. Ein neu verbundener Client erhält zuerst für jede aktive Anfrage eine `started`-Zeile. Verbinden dürfen sich nur Prozesse desselben Benutzers.
- **D-Bus**, gesteuert über `[dbus]`: Busname `io.github.theaifam5.Touchcue` auf dem Sitzungsbus, Objekt `/io/github/theaifam5/Touchcue`, Schnittstelle `io.github.theaifam5.Touchcue1`. Sie bietet die Eigenschaft `Active`, die Methode `ActiveRequests` und die Signale `RequestStarted`, `RequestUpdated` und `RequestEnded`.
- **Kompatibler Socket**, `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, gesteuert über `[compat.maxbaz_socket]`. Er sendet die 5-Byte-Nachrichten von yubikey-touch-detector: `U2F_1`/`U2F_0` für FIDO, `GPG_1`/`GPG_0` für gpg und ssh sowie `MAC_1`/`MAC_0`. Dabei zählen nur Anfragen, die noch auf eine Berührung warten.

`values` enthält ausschließlich diese Platzhalter: alle `request.*`-Schlüssel, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` und `requester.label`. Pfade ausführbarer Dateien, PIDs, UIDs und Befehlszeilen werden nie veröffentlicht. Über `process.chain` und `requester.*` erfährt allerdings jeder Client des Sockets bzw. des Busnamens die Namen der Elternprozesse des Clients, etwa der Shell und der Tools, die ihn gestartet haben.

Ein zweites `touchcue run` beendet sich, solange der Ereignis-Socket oder der Busname belegt ist.
