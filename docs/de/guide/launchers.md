# Launcher

touchcue kann die Aufforderung auch über Programme Ihrer Wahl anzeigen, etwa über rofi, fuzzel, `hyprctl notify` oder ein Statusleisten-Skript. Eigenen Code für diese Programme enthält touchcue nicht. Stattdessen startet ein [Hook mit `until = "ended"`](./hooks#hooks-that-last-for-a-request) das Programm und lässt es laufen, solange die Anfrage besteht. Die Aufforderung erhält das Programm auf zwei Wegen:

- in `TOUCHCUE_TITLE` und `TOUCHCUE_BODY`, also mit dem Text, den auch die Benachrichtigung zeigt – einschließlich `(cancelled)` oder `(timed out)` in der [Sprache](./configuration#locale) der Aufforderung, solange die Aufforderung in der Nachlaufzeit noch stehen bleibt;
- auf stdin als JSON-Zeile, sobald die Aufforderung erscheint und sobald sie verschwindet, mit `on_change = "stream"` außerdem eine Zeile für jede Änderung dazwischen.

Das funktioniert unter Linux.

```toml
[output]
mode = "none"

[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

`mode = "none"` schaltet das Popup ab, sodass der Launcher an seine Stelle tritt. Lassen Sie `[output]` weg, wenn beides erscheinen soll.

## Beispiele

Zu jedem Beispiel gibt es einen Konfigurationsausschnitt und Hinweise:

- [rofi](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/rofi): ein Meldungsdialog pro Anfrage.
- [fuzzel](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/fuzzel): ein leeres Menü, das die Aufforderung zeigt.
- [hyprctl](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/hyprctl): eine Hyprland-Benachrichtigung.
- [waybar](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/waybar): ein Statusleisten-Modul, das die wartenden Anfragen zählt.

## `on_change` wählen

- Für Programme, die einen festen Text anzeigen, etwa rofi oder fuzzel, passt der Standardwert `restart`: Ändert sich ein Wert der Anfrage, beendet touchcue das Programm und startet es mit dem neuen Text neu.
- Programme, die stdin lesen, etwa das waybar-Skript, verwenden `stream` und erhalten jede Änderung als `update`-Zeile.
- Für Programme, die pro Anfrage nur einmal erscheinen sollen, gibt es `ignore`.

Beendet sich ein Programm, bevor seine Anfrage endet – etwa ein Menü, das mit Escape geschlossen wird, oder `hyprctl notify`, das sich sofort beendet –, gilt es als geschlossen. touchcue startet es erst dann wieder, wenn sich ein Wert der Anfrage ändert.

## Nicht vertrauenswürdiger Text

Anwendungs- und Prozessnamen – und damit auch Titel und Text – stammen von den anfragenden Prozessen.

- `command` wird genau so ausgeführt, wie es dasteht; touchcue setzt niemals Werte darin ein. Rufen Sie eine Shell nur ausdrücklich auf, setzen Sie die Variablen in Anführungszeichen und fügen Sie eine Variable nie direkt in den Skripttext ein.
- Ein Wert kann mit `-` beginnen. Übergeben Sie ihn deshalb nach `--`, hängen Sie ihn direkt an seine Option an, wie in `--prompt-only="$TOUCHCUE_BODY"`, oder stellen Sie ihm festen Text voran, wie im hyprctl-Beispiel.
- Programme, die Pango-Markup oder HTML interpretieren, brauchen maskierten Text. Das waybar-Beispiel maskiert ihn, das rofi-Beispiel schaltet Markup gar nicht erst ein.
