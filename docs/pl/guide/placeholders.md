# Symbole zastępcze

Szablony w `[templates]` i `[[rules]]` wstawiają wartości z bieżącego żądania dotknięcia. Reguły w `[[rules]]` dopasowują żądania na podstawie tych samych nazw.

## Składnia szablonów {#template-syntax}

Szablony używają składni Jinja i są renderowane przez [MiniJinja](https://docs.rs/minijinja/3/minijinja/syntax/index.html). Wcześniejsze wersje touchcue używały pojedynczych nawiasów klamrowych, np. `{app.name|"An application"}`; taki szablon jest teraz błędem konfiguracji. Symbol zastępczy w postaci `namespace.field` z poniższej tabeli zapisuje się jako atrybut, np. `app.name`:

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
- Każda wartość jest ciągiem znaków. Nieznana wartość jest niezdefiniowana: renderuje się jako pusty tekst i jest fałszywa, tak samo jak pusty ciąg. Jeśli nie ma aplikacji, `app.name` renderuje się jako pusty tekst.
- Porównania dotyczą ciągów, np. `request.count == "2"`. Aby porównywać liczby, przekonwertuj wartość filtrem `int`: `request.count | int > 1`. Porównanie wypisane bezpośrednio renderuje się jako `True` lub `False`.
- Dostępne są wbudowane [filtry](https://docs.rs/minijinja/3/minijinja/filters/index.html) i [testy](https://docs.rs/minijinja/3/minijinja/tests/index.html), np. filtry `upper`, `lower`, `title`, `capitalize`, `trim`, `replace`, `default`, `first`, `length` i `int` oraz testy `defined`, `startingwith`, `endingwith` i `in`. Filtry `format`, `indent`, `slice` i `batch` są niedostępne. Spośród funkcji globalnych dostępne są `range`, `dict` i `namespace`. Metody, np. `app.name.upper()`, nie są obsługiwane; użyj zamiast nich filtra.
- `{% set %}`, `{% with %}`, `{% for %}` i `{% filter %}` działają. Makra, `include`, `extends` i `import` nie działają — są błędami składni.
- Białe znaki są zachowywane dokładnie tak, jak je zapisano; usuwany jest jedynie jeden znak nowego wiersza na końcu szablonu. Znak `-` wewnątrz ogranicznika bloku, np. `{%-`, `-%}`, `{{-` lub `-}}`, usuwa białe znaki (także znaki nowego wiersza) po tej stronie bloku. Przydaje się to w szablonach zapisanych jako wielowierszowe ciągi TOML.
- Wynik to zwykły tekst; znaki specjalne HTML nie są w nim zamieniane na encje.
:::

::: v-pre
Błąd składni lub nieznana nazwa to błąd konfiguracji, zgłaszany przy jej wczytywaniu ze wskazaniem błędnego fragmentu. Nazwa jest znana, jeśli jest symbolem zastępczym zapisanym jako `namespace.field`, nazwą ustawianą w szablonie przez `set`, `with` lub `for` albo jedną z wymienionych wyżej funkcji globalnych. Cała przestrzeń nazw, np. `{{ app }}` lub `app["name"]`, jest nazwą nieznaną; zapisz `app.name`.
:::

Błąd, który ujawnia się dopiero podczas renderowania, nie sprawia, że monit przepada. Do takich błędów należą: nieznany filtr lub test, operacja na wartościach niewłaściwego typu (np. `request.count + 1`), renderowanie wykonujące ponad 1000 instrukcji lub zapisujące ponad 64 KiB oraz renderowanie trwające dłużej niż 200 ms. Szablon reguły, którego renderowanie się nie powiodło, zostaje zastąpiony szablonem z `[templates]`, a ten z kolei — wbudowanym szablonem domyślnym. Jeśli renderowanie trwa zbyt długo, używane są wbudowane szablony domyślne. Demon zapisuje w logu nazwę szablonu i błąd, najwyżej raz na minutę dla każdego szablonu.

Szablony to zaufana część konfiguracji, którą pisze właściciel pliku konfiguracyjnego. Opisane limity ograniczają liczbę instrukcji i rozmiar wyniku renderowania, ale nie czas ani pamięć zużywane przez pojedynczą instrukcję. Wyrażenie takie jak `"x" * 100000000` tworzy ciąg o rozmiarze 100 MB, a łączenie ciągów operatorem `~` w pętli może podwajać długość ciągu w każdym kroku. Renderowanie, które się nie kończy, działa dalej w tle. Dopóki się nie zakończy, każdy monit korzysta z wbudowanych szablonów, a demon najwyżej raz na minutę zapisuje ostrzeżenie w logu.

## Nazwy

| Przestrzeń nazw | Pola |
| --- | --- |
| `app` | `name`, `id`, `icon`, `exe`, `pid`, `cmdline`, `wm_class`, `container` |
| `process` | `name`, `exe`, `pid`, `cmdline`, `uid`, `chain` |
| `requester` | `name`, `exe`, `pid`, `label` |
| `device` | `vendor`, `model`, `product`, `vid`, `pid`, `kind`, `transport` |
| `request` | `method`, `op`, `action`, `source`, `class`, `confidence`, `elapsed`, `count`, `state`, `detail` |

::: v-pre
`process` to proces klienta, który komunikuje się z urządzeniem lub z gpg-agent, np. `gpg` lub `ssh-sk-helper`. `requester` to program, który zlecił mu to zadanie, np. `claude` uruchamiający `git commit -S`. `app` to aplikacja pulpitu, w której oba działają, np. Kitty. Nieznana wartość jest pusta, dlatego podawaj wartość zapasową, np. `{{ requester.label or process.name or "An application" }}`.
:::

`process.name` to `comm` klienta. `requester.name` i nazwy w `process.chain` to `comm` danego procesu, a jeśli go brak — nazwa jego pliku wykonywalnego. `process.chain` zawiera nazwy procesów od klienta w górę do procesu aplikacji, zaczynając od klienta, np. `gpg ← git ← bash ← claude ← nu ← kitty`. Łańcuch obejmuje najwyżej 8 nazw, każda ma najwyżej 32 znaki. W dłuższym łańcuchu zostaje pierwszych 7 nazw i ostatnia, a pomiędzy nimi `…`, np. `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← … ← kitty`.

## Inicjator {#requester}

Inicjator to program, który uruchomiłeś wewnątrz aplikacji. touchcue przechodzi po procesach nadrzędnych od procesu aplikacji w dół do klienta i wybiera pierwszy proces, którego nie ma na liście pomijanych. Domyślnie ta lista zawiera powłoki, multipleksery terminala, wrappery poleceń (np. `sudo`, `env` i `timeout`) oraz menedżery usług. Własne procesy aplikacji, czyli te, które uruchamiają jej plik wykonywalny, oraz procesy innych użytkowników są pomijane zawsze. Jeśli pominięte zostaną wszystkie procesy, inicjatorem jest klient, a gdy klientem jest sama aplikacja — inicjatora nie ma. [`requester.skip`](./configuration#requester) zastępuje listę, `requester.extend_skip` ją uzupełnia, a `touchcue check` wypisuje listę i pokazuje, jak touchcue widzi terminal, w którym go uruchomiono.

| Drzewo procesów, od klienta | `requester.label` |
| --- | --- |
| `gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← nu ← kitty`, Claude Code w herdr | `claude in kitty` |
| `gpg ← git ← zsh ← kitty`, `git commit -S` wpisane w powłoce | `git in kitty` |
| `ssh-sk-helper ← ssh ← zsh ← kitty`, `ssh` wpisane w powłoce | `ssh in kitty` |
| `gpg ← git ← sh ← make ← bash ← claude ← nu ← kitty` | `claude in kitty` |
| `gpg ← jj ← claude ← nu ← kitty` albo `pass` zamiast `jj` | `claude in kitty` |
| `gpg ← git ← release.sh ← zsh ← kitty`, skrypt uruchomiony przez `bash` | `release.sh in kitty` |
| `gpg ← timeout ← zsh ← kitty` | `gpg in kitty` |
| `git ← nu ← code ← code`, terminal w Visual Studio Code | `git in Visual Studio Code` |
| `firefox`, który sam ma otwarte urządzenie FIDO | `Firefox` |
| `ssh ← restic ← backup.sh ← systemd`, bez aplikacji | `backup.sh` |
| `sudo ← zsh ← kitty`, gdzie klientem jest `sudo` | `sudo in kitty` |
| `gpg ← git ← claude ← zsh ← nvim ← zsh ← kitty`, Claude Code w terminalu Neovima | `nvim in kitty`; z `extend_skip = ["nvim"]`: `claude in kitty` |

Nazwy są porównywane z listą pomijanych na podstawie `comm`, a jeśli `comm` nie da się odczytać — na podstawie nazwy pliku wykonywalnego. Porównanie jest dokładne i rozróżnia wielkość liter, wpis zakończony `*` jest prefiksem, a `comm` mieści najwyżej 15 bajtów, więc dłuższa nazwa wymaga wpisu z prefiksem. Jeśli inicjatora nie ma, `requester.*` jest puste. Jeśli aplikację ustalono na podstawie jej jednostki systemd, przejście zaczyna się od najwyższego procesu tej jednostki, czyli tego, który uruchomił aplikację. Przejście zatrzymuje się na pierwszym procesie, którego nazw nie da się odczytać lub którego PID został użyty ponownie; inicjatorem zostaje wtedy najlepszy kandydat spośród procesów odczytanych wcześniej.

`requester.label` łączy obie nazwy w postaci `<requester.name> in <app.name>` (po polsku `w` zamiast `in`), jeśli obie są znane i różnią się czymś więcej niż wielkością liter ASCII. W przeciwnym razie zawiera tę z nazw `app.name` i `requester.name`, która jest znana, a jeśli żadna nie jest — pusty ciąg. Ma najwyżej 260 znaków. Tak jak nazwy, z których powstaje, jest publikowana klientom IPC i hookom.

Ograniczenia:

- Program, który uruchamia terminal, np. `:terminal` w Neovimie, zostaje wskazany zamiast programu działającego w tym terminalu. Dodaj go do `extend_skip`.
- Skrypt zachowuje własną nazwę tylko wtedy, gdy jest uruchamiany przez wiersz shebang, jako `./release.sh`, a ten wiersz wskazuje interpreter bezpośrednio, np. `#!/bin/bash`. Skrypt uruchomiony jako `bash release.sh` albo z `#!/usr/bin/env bash` ma `comm` równe `bash` i jest pomijany jak powłoka.
- Serwer tmux działający jako demon zmienia proces nadrzędny i nie jest już potomkiem terminala, więc dla poleceń uruchomionych w nim aplikacja nie zostanie ustalona.
- Podobnie jak `app`, inicjator to jedynie deklaracja: każdy proces tego samego użytkownika może nadać sobie dowolne `comm`, np. takie, żeby został pominięty, a wtedy żądanie zostanie przypisane procesowi powyżej.

## Wartości {#values}

| Nazwa | Wartości |
| --- | --- |
| `request.method` | `fido2`, `u2f`, `openpgp` |
| `request.op` | `sign`, `decrypt`, `auth` dla OpenPGP; puste dla FIDO |
| `request.action` | opis słowny tego, na co czeka żądanie, przeznaczony dla ludzi; zobacz niżej |
| `request.source` | `fido`; dla OpenPGP `ssh`, gdy operacja `auth` jest wykonywana, a klient jest połączony z gniazdem ssh gpg-agent, w przeciwnym razie `gpg` |
| `request.class` | `asserted` dla FIDO, `activity` dla OpenPGP |
| `request.state` | `waiting`, `touched`, `cancelled`, `failed`, `timed_out` |
| `request.confidence` | `high`, `medium`, `low` |
| `request.elapsed` | liczba pełnych sekund od rozpoczęcia żądania |
| `request.count` | liczba prób klienta scalonych w to żądanie, licząc od 1 |
| `request.detail` | dodatkowy tekst od zgłaszającego, najwyżej 200 znaków |
| `device.kind` | `fido`, `openpgp` |
| `device.transport` | `usb`, `bluetooth`, `nfc`, `other` |
| `device.vid`, `device.pid` | identyfikator producenta i produktu USB, cztery cyfry szesnastkowe zapisane małymi literami |
| `device.vendor` | nazwa producenta, np. `Yubico`; dla karty OpenPGP jej wytwórca |

`request.confidence` przyjmuje wartość:

- `high`, gdy urządzenie FIDO ma otwarte jeden proces albo gdy żądanie OpenPGP jest kolejną operacją na karcie OpenPGP w ciągu 30 s od zatwierdzenia okna PIN-u lub potwierdzenia w pinentry, a klient tego okna jest nadal połączony z gpg-agent (zobacz [GPG](./gpg#pin-dialog-attribution));
- `medium`, gdy urządzenie FIDO ma otwarte kilka procesów albo gdy dla żądania OpenPGP klient został wybrany spośród klientów gpg-agent w inny sposób;
- `low` w pozostałych przypadkach.

`request.action` opisuje słownie `request.method` i `request.op` na potrzeby domyślnej treści, w języku wybranym przez [`locale`](./configuration#locale). Reguły i hooki powinny porównywać `request.method`, `request.op` i `request.source`, których wartości się nie zmieniają; brzmienie i język `request.action` mogą się zmienić.

| Żądanie | angielski | polski | niemiecki |
| --- | --- | --- | --- |
| FIDO2 lub U2F | `a passkey` | `potwierdzenie kluczem dostępu` | `eine Bestätigung per Passkey` |
| OpenPGP `sign` | `a GPG signature` | `podpis GPG` | `das Signieren mit GPG` |
| OpenPGP `decrypt` | `GPG decryption` | `odszyfrowanie GPG` | `das Entschlüsseln mit GPG` |
| OpenPGP `auth` | `GPG authentication` | `uwierzytelnienie GPG` | `die Authentifizierung mit GPG` |
| OpenPGP z `source` równym `ssh` | `an SSH login` | `logowanie przez SSH` | `eine SSH-Anmeldung` |
| inne operacje OpenPGP | `a GPG operation` | `operację GPG` | `einen GPG-Vorgang` |

W domyślnej treści wersje polska i niemiecka stoją po `czeka na` i `wartet auf`, dlatego są w bierniku.

Źródło FIDO nie odróżnia logowania od rejestracji. Użycie klucza bezpieczeństwa OpenSSH to również żądanie FIDO; reguła dopasowująca `requester.name = "ssh"` może opisać je inaczej.

`request.detail` jest puste, chyba że przesłał je zgłaszający. `touchcue askpass` ustawia je dla kluczy bezpieczeństwa OpenSSH, np. `ED25519-SK SHA256:… → user git`; zobacz [Klucze bezpieczeństwa OpenSSH](./ssh-askpass).

Pole `device.serial` nie istnieje.
