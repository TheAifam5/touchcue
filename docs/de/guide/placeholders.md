# Platzhalter

Vorlagen in `[templates]` und `[[rules]]` setzen Werte aus der aktuellen Berührungsanfrage ein. `[[rules]]` verwenden dieselben Namen für ihre Bedingungen.

## Vorlagensyntax {#template-syntax}

Vorlagen verwenden Jinja-Syntax und werden mit [MiniJinja](https://docs.rs/minijinja/3/minijinja/syntax/index.html) gerendert. Frühere Versionen von touchcue nutzten einfache geschweifte Klammern, etwa `{app.name|"An application"}`; eine solche Vorlage führt inzwischen zu einem Konfigurationsfehler. Platzhalter der Form `namespace.field` aus der Tabelle unten schreiben Sie als Attribut, zum Beispiel `app.name`:

```text
{{ request.method }}                               the value of a placeholder
{{ app.name or process.name or "An application" }} the first value that is not empty, else the literal
{{ requester.name }}{% if app.name %} ({{ app.name }}){% endif %}
                                                   text only when a value is not empty
{% if request.method == "openpgp" %}…{% else %}…{% endif %}
                                                   a branch
{{ requester.name | upper }}                       a filter
{%- if app.name %} … {% endif -%}                  - removes the whitespace before or after a block
{% raw %}{{ }}{% endraw %}                         literal braces
```

::: v-pre
- Alle Werte sind Zeichenketten. Ein unbekannter Wert ist undefiniert: Er wird als leerer Text ausgegeben und gilt als falsch – genau wie eine leere Zeichenkette. Gibt es keine Anwendung, ergibt `app.name` einen leeren Text.
- Vergleiche arbeiten mit Zeichenketten, etwa `request.count == "2"`. Um Zahlen zu vergleichen, wandeln Sie den Wert mit `int` um: `request.count | int > 1`. Gibt man einen Vergleich direkt aus, erscheint `True` oder `False`.
- Die eingebauten [Filter](https://docs.rs/minijinja/3/minijinja/filters/index.html) und [Tests](https://docs.rs/minijinja/3/minijinja/tests/index.html) stehen zur Verfügung, zum Beispiel die Filter `upper`, `lower`, `title`, `capitalize`, `trim`, `replace`, `default`, `first`, `length` und `int` sowie die Tests `defined`, `startingwith`, `endingwith` und `in`. Nicht verfügbar sind die Filter `format`, `indent`, `slice` und `batch`. Von den globalen Funktionen gibt es `range`, `dict` und `namespace`. Methodenaufrufe wie `app.name.upper()` werden nicht unterstützt; verwenden Sie stattdessen einen Filter.
- `{% set %}`, `{% with %}`, `{% for %}` und `{% filter %}` funktionieren. Makros, `include`, `extends` und `import` dagegen nicht: Sie gelten als Syntaxfehler.
- Leerraum bleibt so erhalten, wie er geschrieben ist; nur ein einzelner Zeilenumbruch am Ende der Vorlage wird entfernt. Ein `-` im Begrenzer eines Blocks, also `{%-`, `-%}`, `{{-` oder `-}}`, entfernt auf dieser Seite des Blocks sämtlichen Leerraum einschließlich Zeilenumbrüchen. Das ist praktisch bei Vorlagen, die als mehrzeilige TOML-Zeichenketten geschrieben sind.
- Die Ausgabe ist reiner Text und wird nicht HTML-maskiert.
:::

::: v-pre
Ein Syntaxfehler oder ein unbekannter Name führt beim Laden der Konfiguration zu einem Konfigurationsfehler, der auf die betreffende Stelle verweist. Bekannt ist ein Name, wenn er ein Platzhalter der Form `namespace.field` ist, wenn die Vorlage ihn mit `set`, `with` oder `for` selbst festlegt oder wenn er eine der oben genannten globalen Funktionen ist. Ein ganzer Namensraum, etwa `{{ app }}` oder `app["name"]`, ist nicht bekannt: Schreiben Sie `app.name`.
:::

Fehler, die erst beim Rendern auftreten, kosten nicht die Aufforderung. Dazu gehören ein unbekannter Filter oder Test, eine Operation auf Werten des falschen Typs wie `request.count + 1`, ein Rendervorgang, der mehr als 1.000 Anweisungen ausführt oder mehr als 64 KiB ausgibt, sowie ein Rendervorgang, der länger als 200 ms dauert. Schlägt eine Regelvorlage fehl, springt die Vorlage aus `[templates]` ein, schlägt auch diese fehl, der eingebaute Standard. Dauert ein Rendervorgang zu lange, kommen die eingebauten Standards zum Einsatz. Der Daemon protokolliert den Namen der fehlgeschlagenen Vorlage und den Fehler, höchstens einmal pro Minute und Vorlage.

Vorlagen gelten als vertrauenswürdige Konfiguration, die der Eigentümer der Konfigurationsdatei verfasst. Die genannten Grenzen beschränken die Anzahl der Anweisungen und die Ausgabemenge eines Rendervorgangs, nicht aber die Zeit oder den Speicher, die eine einzelne Anweisung beansprucht: Ein Ausdruck wie `"x" * 100000000` erzeugt eine Zeichenkette von 100 MB, und wer in einer Schleife Zeichenketten mit `~` verkettet, kann eine davon mit jedem Schritt verdoppeln. Ein Rendervorgang, der nicht zum Ende kommt, läuft im Hintergrund weiter. Bis er abgeschlossen ist, verwenden alle Aufforderungen die eingebauten Vorlagen, und der Daemon protokolliert höchstens einmal pro Minute eine Warnung.

## Namen

| Namensraum | Felder |
| --- | --- |
| `app` | `name`, `id`, `icon`, `exe`, `pid`, `cmdline`, `wm_class`, `container` |
| `process` | `name`, `exe`, `pid`, `cmdline`, `uid`, `chain` |
| `requester` | `name`, `exe`, `pid`, `label` |
| `device` | `vendor`, `model`, `product`, `vid`, `pid`, `kind`, `transport` |
| `request` | `method`, `op`, `action`, `source`, `class`, `confidence`, `elapsed`, `count`, `state`, `detail` |

::: v-pre
`process` ist der Client-Prozess, der mit dem Gerät oder mit gpg-agent kommuniziert, etwa `gpg` oder `ssh-sk-helper`. `requester` ist das Programm, das ihn dazu veranlasst hat, etwa `claude`, das `git commit -S` ausführt. `app` ist die Desktop-Anwendung, in der beide laufen, etwa Kitty. Unbekannte Werte sind leer; geben Sie daher einen Ersatzwert an, wie in `{{ requester.label or process.name or "An application" }}`.
:::

`process.name` ist das `comm` des Clients. `requester.name` und die Namen in `process.chain` sind jeweils das `comm` des Prozesses, ersatzweise der Dateiname seiner ausführbaren Datei. `process.chain` führt die Namen vom Client hinauf bis zum Prozess der Anwendung auf, beginnend mit dem Client, zum Beispiel `gpg ← git ← bash ← claude ← nu ← kitty` – höchstens 8 Namen mit je höchstens 32 Zeichen. Bei einer längeren Kette bleiben die ersten 7 Namen und der letzte erhalten, dazwischen steht `…`, etwa `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← … ← kitty`.

## Urheber {#requester}

Der Urheber ist das Programm, das Sie innerhalb der Anwendung gestartet haben. Um ihn zu ermitteln, geht touchcue die Elternprozesse vom Prozess der Anwendung abwärts bis zum Client durch und nimmt den ersten Prozess, der nicht auf der Ausschlussliste steht. Standardmäßig stehen darauf Shells, Terminal-Multiplexer, Befehls-Wrapper wie `sudo`, `env` und `timeout` sowie Dienstmanager. Die eigenen Prozesse der Anwendung – also solche, die ihre ausführbare Datei ausführen – und Prozesse anderer Benutzer werden immer übersprungen. Werden alle Prozesse übersprungen, ist der Client der Urheber; ist der Client die Anwendung selbst, gibt es keinen Urheber. [`requester.skip`](./configuration#requester) ersetzt die Liste, `requester.extend_skip` ergänzt sie, und `touchcue check` gibt die Liste aus und zeigt, wie touchcue sein eigenes Terminal auswertet.

| Prozessbaum, beginnend mit dem Client | `requester.label` |
| --- | --- |
| `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← nu ← kitty`, Claude Code in herdr | `claude in kitty` |
| `gpg ← git ← zsh ← kitty`, `git commit -S` in einer Shell eingegeben | `git in kitty` |
| `ssh-sk-helper ← ssh ← zsh ← kitty`, `ssh` in einer Shell eingegeben | `ssh in kitty` |
| `gpg ← git ← sh ← make ← bash ← claude ← nu ← kitty` | `claude in kitty` |
| `gpg ← jj ← claude ← nu ← kitty`, oder `pass` statt `jj` | `claude in kitty` |
| `gpg ← git ← release.sh ← zsh ← kitty`, ein von `bash` ausgeführtes Skript | `release.sh in kitty` |
| `gpg ← timeout ← zsh ← kitty` | `gpg in kitty` |
| `git ← nu ← code ← code`, das Terminal von Visual Studio Code | `git in Visual Studio Code` |
| `firefox`, das das FIDO-Gerät selbst geöffnet hat | `Firefox` |
| `ssh ← restic ← backup.sh ← systemd`, ohne Anwendung | `backup.sh` |
| `sudo ← zsh ← kitty`, mit `sudo` als Client | `sudo in kitty` |
| `gpg ← git ← claude ← zsh ← nvim ← zsh ← kitty`, Claude Code in einem Neovim-Terminal | `nvim in kitty`; mit `extend_skip = ["nvim"]` `claude in kitty` |

Für den Abgleich mit der Ausschlussliste dient `comm`, ersatzweise der Dateiname der ausführbaren Datei, falls sich `comm` nicht lesen lässt. Verglichen wird exakt und unter Beachtung der Groß- und Kleinschreibung; ein Eintrag mit abschließendem `*` gilt als Präfix. Da `comm` höchstens 15 Byte umfasst, braucht ein längerer Name einen Präfix-Eintrag. Gibt es keinen Urheber, ist `requester.*` leer. Wird die Anwendung über ihre systemd-Unit gefunden, beginnt der Durchlauf auf Anwendungsseite beim obersten Prozess dieser Unit, also dem, der die Anwendung gestartet hat. Der Durchlauf endet beim ersten Prozess, dessen Namen sich nicht lesen lassen oder dessen PID inzwischen neu vergeben wurde; als Urheber gilt dann die beste Vermutung unter den bis dahin gelesenen Prozessen.

`requester.label` kombiniert beide Namen: `<requester.name> in <app.name>` (auf Polnisch `w` statt `in`), wenn beide bekannt sind und sich nicht nur in der ASCII-Groß- und Kleinschreibung unterscheiden. Andernfalls steht dort der bekannte der beiden Namen `app.name` und `requester.name`; ist keiner bekannt, bleibt das Feld leer. Es umfasst höchstens 260 Zeichen. Wie die Namen, aus denen es gebildet wird, wird es an IPC-Clients und Hooks veröffentlicht.

Grenzen:

- Ein Programm, das ein Terminal bereitstellt, etwa Neovim mit `:terminal`, wird anstelle dessen genannt, was in diesem Terminal läuft. Nehmen Sie es in `extend_skip` auf.
- Ein Skript behält seinen eigenen Namen nur, wenn es über seine Shebang-Zeile gestartet wird, also als `./release.sh`, und diese Zeile den Interpreter direkt nennt, etwa `#!/bin/bash`. Wird es als `bash release.sh` oder mit `#!/usr/bin/env bash` gestartet, lautet sein `comm` `bash`, und es wird wie eine Shell übersprungen.
- Ein tmux-Server, der als Daemon läuft, wird vom Terminal abgekoppelt und einem anderen Elternprozess zugeordnet. Befehle, die darin laufen, werden deshalb ohne die Anwendung gefunden.
- Wie `app` ist auch der Urheber nur eine Behauptung: Jeder Prozess desselben Benutzers kann sein `comm` frei wählen, etwa um übersprungen zu werden. Die Anfrage wird dann dem Prozess darüber zugeordnet.

## Werte {#values}

| Name | Werte |
| --- | --- |
| `request.method` | `fido2`, `u2f`, `openpgp` |
| `request.op` | `sign`, `decrypt`, `auth` bei OpenPGP; leer bei FIDO |
| `request.action` | worauf die Anfrage wartet, in lesbarer Form; siehe unten |
| `request.source` | `fido`; bei OpenPGP `ssh`, wenn ein `auth`-Vorgang läuft, während ein Client mit dem ssh-Socket von gpg-agent verbunden ist, sonst `gpg` |
| `request.class` | `asserted` bei FIDO, `activity` bei OpenPGP |
| `request.state` | `waiting`, `touched`, `cancelled`, `failed`, `timed_out` |
| `request.confidence` | `high`, `medium`, `low` |
| `request.elapsed` | volle Sekunden seit Beginn der Anfrage |
| `request.count` | Anzahl der Versuche des Clients, die in dieser Anfrage zusammengefasst sind, beginnend bei 1 |
| `request.detail` | Zusatztext eines meldenden Programms, höchstens 200 Zeichen |
| `device.kind` | `fido`, `openpgp` |
| `device.transport` | `usb`, `bluetooth`, `nfc`, `other` |
| `device.vid`, `device.pid` | USB-Hersteller- und -Produkt-ID, vier Hexadezimalziffern in Kleinbuchstaben |
| `device.vendor` | Herstellername, zum Beispiel `Yubico`; bei einer OpenPGP-Karte deren Hersteller |

`request.confidence` ist `high`, wenn genau ein Prozess das FIDO-Gerät geöffnet hat, oder bei einer OpenPGP-Anfrage, die der nächste OpenPGP-Kartenvorgang innerhalb von 30 s nach einem bestätigten PIN- oder Bestätigungsdialog von pinentry ist, dessen Client noch mit gpg-agent verbunden ist (siehe [GPG](./gpg#pin-dialog-attribution)). Der Wert ist `medium`, wenn mehrere Prozesse das FIDO-Gerät geöffnet haben oder wenn der Client einer OpenPGP-Anfrage auf andere Weise unter den Clients von gpg-agent ausgewählt wird. In allen übrigen Fällen ist er `low`.

`request.action` beschreibt `request.method` und `request.op` in Worten für den Standardtext, und zwar in der Sprache von [`locale`](./configuration#locale). Regeln und Hooks sollten dagegen `request.method`, `request.op` und `request.source` vergleichen, deren Werte sich nicht ändern; Formulierung und Sprache von `request.action` können sich ändern.

| Anfrage | Englisch | Polnisch | Deutsch |
| --- | --- | --- | --- |
| FIDO2 oder U2F | `a passkey` | `potwierdzenie kluczem dostępu` | `eine Bestätigung per Passkey` |
| OpenPGP `sign` | `a GPG signature` | `podpis GPG` | `das Signieren mit GPG` |
| OpenPGP `decrypt` | `GPG decryption` | `odszyfrowanie GPG` | `das Entschlüsseln mit GPG` |
| OpenPGP `auth` | `GPG authentication` | `uwierzytelnienie GPG` | `die Authentifizierung mit GPG` |
| OpenPGP mit `source` `ssh` | `an SSH login` | `logowanie przez SSH` | `eine SSH-Anmeldung` |
| andere OpenPGP-Vorgänge | `a GPG operation` | `operację GPG` | `einen GPG-Vorgang` |

Die polnischen und deutschen Formulierungen folgen im Standardtext auf `czeka na` bzw. `wartet auf` und stehen daher im Akkusativ.

Die FIDO-Quelle kann eine Anmeldung nicht von einer Registrierung unterscheiden. Auch ein OpenSSH-Sicherheitsschlüssel erzeugt eine FIDO-Anfrage; mit einer Regel, die auf `requester.name = "ssh"` prüft, lässt sie sich anders formulieren.

`request.detail` ist leer, sofern kein meldendes Programm einen Text geschickt hat. `touchcue askpass` setzt es für OpenSSH-Sicherheitsschlüssel, zum Beispiel auf `ED25519-SK SHA256:… → user git`; siehe [OpenSSH-Sicherheitsschlüssel](./ssh-askpass).

Ein Feld `device.serial` gibt es nicht.
