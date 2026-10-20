# Konfiguracja

touchcue odczytuje `$XDG_CONFIG_HOME/touchcue/config.toml`, a jeśli ta zmienna nie jest ustawiona — `~/.config/touchcue/config.toml`. Opcja `--config <PATH>` wskazuje inny plik, który w takim przypadku musi istnieć. Jeśli domyślnego pliku nie ma, obowiązują wartości domyślne opisane poniżej.

Wszystkie klucze są opcjonalne. Nieznane klucze i wartości spoza zakresu to błędy: `touchcue run` się nie uruchomi, a `touchcue check` je zgłosi. W obu przypadkach, o ile to możliwe, podawane jest miejsce błędu w pliku.

## Przykład

Wszystkie wartości domyślne zapisane jawnie:

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

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `locale` | nazwa ustawień regionalnych, np. `pl`, `de` lub `pl_PL.UTF-8` | ze środowiska |

`locale` wybiera język wbudowanych tekstów monitu: domyślnego tytułu i treści, [`request.action`](./placeholders#values) oraz dopisków `(cancelled)` i `(timed out)`. Dostępne są angielski, polski (`pl`) i niemiecki (`de`). Jeśli `locale` nie jest ustawiony lub jest pusty, `touchcue run` przy starcie jednorazowo odczytuje pierwszą niepustą ze zmiennych `LC_ALL`, `LC_MESSAGES` i `LANG`. Językiem jest część nazwy przed pierwszym znakiem `_`, `.` lub `@`; `C`, `POSIX` i inne języki oznaczają angielski. Rozstrzyga pierwsza niepusta wartość, więc przy nieznanym `locale` touchcue nie sięga do środowiska. Szablony, które piszesz sam, nie są tłumaczone. `locale` jest kluczem najwyższego poziomu, dlatego musi stać w pliku przed pierwszą sekcją `[section]`:

```toml
locale = "pl"

[output]
mode = "popup"
```

## `[output]`

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `mode` | `popup`, `notification`, `both`, `none`, `command` | `popup` |
| `fallback` | jak wyżej | `notification` |

touchcue używa trybu `mode`, jeśli pulpit go obsługuje, w przeciwnym razie `fallback`, a jeśli i ten nie jest obsługiwany, niczego nie pokazuje.

- `popup` wyświetla nakładane okienko, które nie przejmuje fokusu. Korzysta z layer-shell Waylanda, gdy ustawiona jest zmienna `WAYLAND_DISPLAY` lub `WAYLAND_SOCKET`, a kompozytor obsługuje ten protokół. W przeciwnym razie używa X11, o ile ustawiono `DISPLAY` (dotyczy to również XWaylanda). Gdy połączenie z serwerem wyświetlania zostanie zerwane, na przykład po restarcie kompozytora, touchcue łączy się ponownie tak samo jak przy starcie: przez `WAYLAND_DISPLAY`, a w drugiej kolejności przez `DISPLAY`. Pierwsza próba następuje po 1 sekundzie, kolejne coraz rzadziej, aż do jednej na minutę. Po ponownym połączeniu monity, które nadal powinny być widoczne, pojawiają się znowu. touchcue nie wykryje kompozytora, który wróci pod inną nazwą gniazda, i może wtedy połączyć się przez `DISPLAY` z XWaylandem.
- `notification` wysyła powiadomienie pulpitu przez D-Bus. Rysuje je serwer powiadomień, na zrzucie poniżej jest to mako:

  ![Powiadomienie mako: „Touch your security key”, „Browser is waiting for a passkey”](/screenshots/notification.png)
- `both` pokazuje jednocześnie okienko i powiadomienie.
- `none` niczego nie pokazuje.
- `command` jest akceptowany, ale pomijany z ostrzeżeniem. Jeśli chcesz pokazywać monity w programach takich jak rofi czy fuzzel, użyj [hooków z `until`](./hooks#hooks-that-last-for-a-request); zobacz też [Launchery](./launchers).

## `[popup]`

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `position` | `center`, `top-left`, `top-right`, `bottom-left`, `bottom-right`, `top`, `bottom` | `center` |
| `output` | `"focused"`, `"all"`, `"cursor"` lub lista nazw wyjść | `"focused"` |
| `min_display_ms` | od 0 do 600000 | 800 |
| `show_delay_ms` | od 0 do 600000 | 0 |
| `modal` | `true`, `false` | `false` |
| `modal_dim` | od 0.0 do 1.0 | 0.4 |
| `modal_dismiss` | `true`, `false` | `true` |
| `animations` | `true`, `false` | `true` |

![Okienko w prawym górnym rogu wyjścia przy position = "top-right"](/screenshots/corner.png)

Okienko jest rysowane w skali wyjścia, na którym się znajduje. Jeśli kompozytor Waylanda obsługuje fractional-scale-v1, możliwe są skale ułamkowe, np. 1,5; w przeciwnym razie touchcue używa preferowanej przez kompozytor skali całkowitej. W X11 skala wynika z `Xft.dpi`.

`min_display_ms` określa, jak długo co najmniej pozostaje widoczne okienko, które już się pojawiło. `show_delay_ms` określa, jak długo żądanie musi czekać, zanim pojawi się jego okienko.

![Domyślne, wyśrodkowane okienko: „Touch your security key”, „Browser is waiting for a passkey”](/screenshots/default.png)

Kilka okienek na jednym wyjściu układa się w kolejności, w jakiej rozpoczęły się żądania: kolejne odsuwają się od krawędzi ekranu, a przy `center` układają się pionowo wokół środka.

![Dwa okienka jedno nad drugim: jedno od przeglądarki, drugie od claude w terminalu](/screenshots/stacked.png)

### Nakładka stanu {#state-overlay}

Ikona aplikacji jest przyciemniona, a nad jej środkiem widać stan żądania. Okienko bez ikony pokazuje w jej miejscu sam stan.

- Dopóki żądanie czeka na dotknięcie, nad ikoną obraca się wskaźnik oczekiwania.
- Gdy operacja się zakończy, wskaźnik zmienia się w symbol na kolorowym kółku: zielony ptaszek po dotknięciu, czerwony krzyżyk po anulowaniu lub błędzie, bursztynowy zegar po przekroczeniu limitu czasu.

  ![Okienko, w którym wskaźnik oczekiwania obraca się nad przyciemnioną ikoną przeglądarki, a po dotknięciu zmienia się w ptaszek na zielonym kółku](/screenshots/anim-touched.gif)

  ![Okienko, w którym wskaźnik oczekiwania po anulowaniu zmienia się w krzyżyk na czerwonym kółku](/screenshots/anim-cancelled.gif)

  ![Okienko, w którym wskaźnik oczekiwania po przekroczeniu limitu czasu zmienia się w zegar na bursztynowym kółku](/screenshots/anim-timed-out.gif)
- Gdy klient ponowi próbę, symbol znów zmienia się we wskaźnik oczekiwania.

  ![Okienko, w którym wskaźnik oczekiwania zmienia się w zegar, po ponowieniu próby przez klienta znów się obraca, a na koniec zmienia się w ptaszek dotknięcia](/screenshots/anim-retry.gif)

Każda zmiana trwa 250 ms: nowy element pojawia się płynnie i powiększa do docelowego rozmiaru. Przy `animations = false` lub gdy pulpit żąda ograniczenia animacji, wskaźnik oczekiwania jest nieruchomy, a symbole pojawiają się od razu. touchcue odczytuje to ustawienie pulpitu raz, przy starcie, przez portal pulpitu XDG: `reduced-motion` = 1 w `org.freedesktop.appearance` lub `enable-animations` = false w `org.gnome.desktop.interface`, jeśli portal udostępnia to ustawienie. Bez portalu animacje nie są ograniczane.

### Wyjścia

`output` określa, na których monitorach pojawia się okienko. Wyjście jest wybierane w chwili pojawienia się okienka.

- `"focused"` zostawia wybór kompozytorowi Waylanda, który zwykle wskazuje aktywny monitor. W X11 jest to monitor pod wskaźnikiem myszy.
- `"all"` pokazuje okienko na każdym wyjściu.

  ![To samo okienko wyśrodkowane na każdym z dwóch wyjść](/screenshots/all-outputs.png)
- `"cursor"` wybiera wyjście pod wskaźnikiem myszy. W Waylandzie działa to tylko w Hyprlandzie, przez jego gniazdo IPC. W innych środowiskach albo gdy zapytanie się nie powiedzie, touchcue zapisuje ostrzeżenie w logu i używa `"focused"`. W X11 działa tak samo jak `"focused"`.
- Lista, np. `["DP-1", "HDMI-A-1"]`, pokazuje okienko na każdym wymienionym wyjściu, które istnieje. W Waylandzie nazwy wyjść podaje kompozytor (`hyprctl monitors`, `swaymsg -t get_outputs`), a w X11 RandR (`xrandr --listmonitors`). Jeśli żadne z wymienionych wyjść nie istnieje, touchcue zapisuje ostrzeżenie w logu i używa `"focused"`. Pusta lista lub pusta nazwa to błąd.

### Okienka modalne

Przy `modal = true` okienko, które czeka na dotknięcie, blokuje także kliknięcia: na każdym jego wyjściu pod okienkiem pojawia się pełnoekranowa warstwa, która przechwytuje wszystkie kliknięcia. Klawiatura nigdy nie jest przechwytywana, więc to, co wpisujesz, nadal trafia do aktywnego okna. Nakładka znika, gdy na danym wyjściu nie czeka już żadne żądanie, a najpóźniej po 120 sekundach, nawet jeśli żądania nadal czekają. Żądania, które pojawią się w trakcie jej wyświetlania, nie przedłużają tego czasu. Po upływie tych 120 sekund wyjście przez 30 sekund nie dostaje nowej nakładki.

![Okienko modalne nad przyciemnionym wyjściem](/screenshots/modal-dim.png)

- `modal_dim` określa stopień przyciemnienia nakładki, maksymalnie `1.0` (czerń). Przy `0.0` kliknięcia są blokowane bez przyciemniania:

  ![Okienko modalne przy modal_dim = 0.0: wyjście nie jest przyciemnione](/screenshots/modal-nodim.png)
- Przy `modal_dismiss = true` kliknięcie lub dotknięcie nakładki ukrywa okienka i nakładki żądań na tym wyjściu. Ukrywane jest tylko to, co widać na ekranie: żądanie nadal czeka na dotknięcie. Kolejne żądanie na tym wyjściu od razu dostaje nową nakładkę. Nakładkę zamyka tylko lewy, środkowy lub prawy przycisk myszy; przewijanie i przyciski boczne jej nie zamykają.
- Przy `modal_dismiss = false` kolejne żądania mogą utrzymywać przyciemnienie wyjścia dłużej niż 120 sekund. Limit dotyczy bowiem każdej nakładki osobno, a nowe żądanie dostaje nową nakładkę, gdy poprzednia zniknęła albo 30 sekund po tym, jak poprzednia osiągnęła limit. Klawiatura przez cały czas działa normalnie.

W X11 nakładka blokuje kliknięcia, ale niczego nie przyciemnia; `modal_dim` nie ma tam znaczenia. W Waylandzie nakładka leży w warstwie overlay protokołu layer-shell, ponad oknami i panelami.

## `[notification]`

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `urgency` | `low`, `normal`, `critical` | `critical` |
| `safety_timeout_s` | od 0 do 600 | 60 |

`safety_timeout_s` to czas, po którym serwer powiadomień wycofuje powiadomienie, nawet jeśli żądanie nadal czeka. touchcue przekazuje tę wartość jako czas wygaśnięcia powiadomienia; `0` oznacza, że powiadomienie nie wygasa.

## `[icons]` {#icons}

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `theme` | nazwa katalogu motywu ikon, np. `Papirus-Dark` | wykrywany automatycznie |

Ikony aplikacji z plików .desktop oraz nazwy ikon z [`[[rules]]`](#rules) są wyszukiwane w rozmiarze 64 pikseli zgodnie ze [specyfikacją motywów ikon freedesktop](https://specifications.freedesktop.org/icon-theme/latest/): najpierw w tym motywie, potem w motywach, po których on dziedziczy, a na końcu w `hicolor`. Motywów touchcue szuka kolejno w `~/.icons`, w `icons` pod `$XDG_DATA_HOME` (gdy zmienna nie jest ustawiona: `~/.local/share`) i pod każdym katalogiem z `$XDG_DATA_DIRS` (gdy zmienna nie jest ustawiona: `/usr/local/share:/usr/share`), a na końcu w `/usr/share/pixmaps`. Motyw opisuje pierwszy znaleziony dla niego plik `index.theme`. Jeśli ikony nie ma w żadnym motywie, touchcue szuka w tych katalogach pliku o jej nazwie. Używane są tylko pliki PNG i SVG, a końcówka `.png`, `.svg` lub `.xpm` w nazwie ikony (jak w `Icon=app.png` w niektórych plikach .desktop) jest ignorowana. Wynik każdego wyszukiwania nazwy jest zapamiętywany. Nazwa, której nie znaleziono w żadnym motywie, nie jest szukana ponownie, dopóki demon nie zostanie zrestartowany albo dopóki nie wyszuka się ponad 512 nazw. Przy każdym użyciu znalezionego pliku touchcue sprawdza, czy ten plik nadal istnieje. Wartość pusta, `.`, `..` lub zawierająca `/` albo znaki sterujące to błąd konfiguracji.

Jeśli `theme` nie jest ustawione, `touchcue run` wykrywa motyw raz, przy starcie. Wybiera pierwsze z poniższych źródeł, które wskazuje zainstalowany motyw:

1. ustawienia portalu pulpitu XDG: `icon-theme` z `org.gnome.desktop.interface`, a następnie `Theme` z `org.kde.kdeglobals.Icons`;
2. `gtk-icon-theme-name` w grupie `[Settings]` pliku `~/.config/gtk-4.0/settings.ini`, a następnie pliku `~/.config/gtk-3.0/settings.ini`;
3. `Theme` w grupie `[Icons]` pliku `~/.config/kdeglobals`;
4. `icon_theme` w grupie `[Appearance]` pliku `~/.config/qt6ct/qt6ct.conf`, a następnie pliku `~/.config/qt5ct/qt5ct.conf`;
5. `hicolor`.

Jeśli ustawiono `$XDG_CONFIG_HOME`, `~/.config` oznacza katalog z tej zmiennej. Na pulpicie, którego `XDG_CURRENT_DESKTOP` zawiera `KDE`, ustawienie portalu KDE jest odczytywane przed ustawieniem GNOME. Portal jest pomijany, jeśli żaden portal nie jest zainstalowany ani uruchomiony na magistrali sesji albo jeśli połączenie z nim, jego uruchomienie i odczytanie obu ustawień trwa dłużej niż 1,5 s. Całe wykrywanie, łącznie z wczytaniem wybranego motywu, trwa najwyżej 3 s; po tym czasie demon startuje z `hicolor` i zapisuje ostrzeżenie w logu. Skonfigurowany lub wykryty motyw, który nie jest zainstalowany, zostaje pominięty z ostrzeżeniem i touchcue sprawdza kolejne źródło; po skonfigurowanym motywie następuje opisane wyżej wykrywanie. Motyw zmieniony w ustawieniach pulpitu zacznie obowiązywać po restarcie demona. `touchcue check` wypisuje motyw i jego źródło, np. `icons: theme = "Papirus-Dark" (portal org.gnome.desktop.interface)`.

## `[templates]`

::: v-pre
| Klucz | Domyślnie |
| --- | --- |
| `title` | `Touch {{ device.vendor or "your security key" }}` |
| `body` | `{{ requester.label or process.name or "An application" }} is waiting for {{ request.action }}` |
:::

Są to domyślne szablony angielskie. Gdy [`locale`](#locale) wskazuje polski lub niemiecki, domyślne szablony wyglądają tak:

::: v-pre
| Język | `title` | `body` |
| --- | --- | --- |
| polski | `Dotknij klucza {{ device.vendor or "bezpieczeństwa" }}` | `{{ requester.label or process.name or "Aplikacja" }} czeka na {{ request.action }}` |
| niemiecki | `Bitte {{ device.vendor ~ "-Schlüssel" if device.vendor else "Sicherheitsschlüssel" }} berühren` | `{{ requester.label or process.name or "Eine Anwendung" }} wartet auf {{ request.action }}` |
:::

Domyślna treść wskazuje inicjatora i jego aplikację za pomocą [`requester.label`](./placeholders#requester), np. `claude in Kitty is waiting for a GPG signature`, a gdy o dotknięcie prosi sama aplikacja — `Firefox is waiting for a passkey`. [`request.action`](./placeholders#values) opisuje słownie, na co czeka żądanie.

Szablony używają składni Jinja i [symboli zastępczych](./placeholders#template-syntax). Jeśli renderowanie szablonu się nie powiedzie, touchcue użyje pierwszego dostępnego z następujących: szablonu z pasującej reguły, szablonu z `[templates]`, powyższego szablonu domyślnego, pustego tekstu. Jeśli żądanie zakończyło się bez dotknięcia, jego monit pozostaje jeszcze przez chwilę widoczny: okienko pokazuje wynik w [nakładce stanu](#state-overlay), a powiadomienie dopisuje do treści `(cancelled)` lub `(timed out)`, po polsku `(anulowano)` lub `(upłynął limit czasu)`, a po niemiecku `(abgebrochen)` lub `(Zeitüberschreitung)`.

Tytuł zajmuje najwyżej dwa wiersze, a treść najwyżej sześć; dłuższy tekst jest obcinany i kończy się wielokropkiem:

![Okienko z długim tytułem i treścią, które są zawijane i kończą się wielokropkiem](/screenshots/long-text.png)

![Trzy okienka z tym samym tekstem: dotknięte żądanie z ptaszkiem, anulowane z krzyżykiem i z przekroczonym limitem czasu z zegarem](/screenshots/outcomes.png)

## `[[rules]]` {#rules}

Reguły zmieniają monit dla pasujących żądań. Stosowana jest pierwsza reguła, której wszystkie wpisy `match` są równe wartościom symboli zastępczych żądania; kolejne reguły są wtedy ignorowane.

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

| Klucz | Typ | Znaczenie |
| --- | --- | --- |
| `match` | tabela ciągów | Nazwy symboli zastępczych i ich wartości. Wymagany i nie może być pusty. Wartości są porównywane dokładnie, z rozróżnianiem wielkości liter; symbol zastępczy bez wartości nigdy nie pasuje. |
| `title` | szablon | Zastępuje `templates.title`. |
| `body` | szablon | Zastępuje `templates.body`. |
| `icon` | ścieżka lub nazwa | Ikona wyświetlana zamiast ikony aplikacji: ścieżka bezwzględna, ścieżka w katalogu domowym zaczynająca się od `~/` albo nazwa ikony. |
| `suppress` | wartość logiczna | Dla pasujących żądań nic nie jest wyświetlane, ale IPC nadal je publikuje. Domyślnie `false`. |

Ścieżka w `icon` musi wskazywać plik PNG lub SVG. Ścieżka zaczynająca się od `~/` nie może wychodzić poza katalog domowy: bezwzględna reszta ścieżki (jak w `~//etc`) albo element `..` to błąd konfiguracji. Wartość bez `/`, np. `utilities-terminal`, jest nazwą ikony i jest wyszukiwana w [motywie ikon](#icons) tak samo jak ikony aplikacji. Każda inna ścieżka względna to błąd konfiguracji. Ścieżki ikon są sprawdzane za każdym razem, gdy monit się pojawia lub zmienia, a na znalezienie pliku jest 0,5 s. Pod uwagę brane są również pliki utworzone po starcie demona. Nazwa ikony, której nie znaleziono, pozostaje nieznaleziona do restartu demona albo do wyszukania ponad 512 nazw, jak opisano w sekcji [`[icons]`](#icons). Jeśli w tym czasie nie uda się znaleźć pliku, monit pokazuje ikonę aplikacji, a demon zapisuje ostrzeżenie w logu.

![Okienko z tytułem i ikoną klucza z reguły pod okienkiem z ikoną aplikacji](/screenshots/rule-icon.png)

Wyświetlona ikona nie dowodzi, który program poprosił o dotknięcie. Każdy program działający z uprawnieniami twojego użytkownika może umieścić łudząco podobne ikony w `~/.icons` lub `~/.local/share/icons` albo zainstalować pliki .desktop wskazujące ikonę innej aplikacji.

## `[[hooks]]` {#hooks}

Hooki uruchamiają polecenie przy zdarzeniach touchcue. Zdarzenia, środowisko przekazywane poleceniu i gotowe przepisy opisuje strona [Hooki](./hooks). Jeśli używasz hooków, ten plik uruchamia polecenia z uprawnieniami twojego użytkownika. Zadbaj więc, aby tylko ty mógł go modyfikować, i nigdy nie uruchamiaj demona jako root z plikiem konfiguracyjnym, który może zmienić inny użytkownik.

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "fido2" }
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
timeout_ms = 5000
concurrency = 4
```

| Klucz | Typ | Znaczenie |
| --- | --- | --- |
| `on` | lista nazw zdarzeń | Zdarzenia, które uruchamiają polecenie. Wymagany i nie może być pusty. Nieznana nazwa to błąd. |
| `match` | tabela ciągów | Nazwy symboli zastępczych i wartości, które muszą się zgadzać, tak jak w `[[rules]]`. Opcjonalny; bez niego polecenie uruchamia każde wymienione zdarzenie. Zdarzenia urządzeń mają tylko wartości `device.*`, a zdarzenia demona nie mają żadnych. Jeśli `match` wymaga wartości, których zdarzenie nie ma, polecenie nie zostanie uruchomione. `match` służy wyłącznie do wybierania zdarzeń: nazwy aplikacji i procesów ustawiają same procesy. |
| `command` | lista ciągów | Program i jego argumenty, uruchamiane bez powłoki. Wymagany; nazwa programu nie może być pusta. |
| `timeout_ms` | od 1 do 600000 | Czas, po którym działające polecenie zostaje zakończone. Domyślnie 5000. Niedozwolony razem z `until`. |
| `concurrency` | od 1 do 32 | Maksymalna liczba jednocześnie działających uruchomień tego hooka. Domyślnie 4. Niedozwolony razem z `until`. |
| `until` | `ended` | Zamiast jednego uruchomienia na zdarzenie uruchamia jeden proces na żądanie, działający do końca żądania. `on` może wtedy zawierać tylko `started` i `waiting`. Zobacz [Hooki działające przez całe żądanie](./hooks#hooks-that-last-for-a-request). |
| `on_change` | `restart`, `stream`, `ignore` | Z `until`: jak zmiana żądania wpływa na jego proces. Domyślnie `restart`. |
| `stop_signal` | `SIGTERM`, `SIGINT`, `SIGHUP` | Z `until`: sygnał, który zatrzymuje proces. Domyślnie `SIGTERM`. |
| `stop_grace_ms` | od 0 do 2000 | Z `until`: czas między `stop_signal` a `SIGKILL`. Domyślnie 1000. |
| `restart_interval_ms` | od 0 do 600000 | Z `until`: minimalny odstęp między dwoma uruchomieniami procesu dla tego samego żądania. Domyślnie 200. |

## `[sources.fido]`

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `enabled` | wartość logiczna | `true` |
| `keepalive_timeout_ms` | od 100 do 600000 | 1500 |
| `retry_window_ms` | od 1 do 600000 | 1000 |

`keepalive_timeout_ms` określa, jak długo żądanie FIDO trwa bez komunikatu keepalive od klucza. `retry_window_ms` określa, jak długo zakończona operacja czeka na ponowienie próby przez klienta. Jeśli klient ponowi próbę w tym czasie, wyświetlany jest nadal ten sam monit.

## `[sources.gpg]`

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `enabled` | wartość logiczna | `true` |

Włącza gniazdo pomocnicze `$XDG_RUNTIME_DIR/touchcue/helper.sock`, przez które [wrapper gpg-agent](./gpg) i [`touchcue askpass`](./ssh-askpass) zgłaszają operacje.

## `[requester]` {#requester}

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `skip` | lista nazw procesów | powłoki, multipleksery, wrappery i menedżery usług, jak w przykładzie powyżej |
| `extend_skip` | lista nazw procesów | `[]` |

Te listy wskazują procesy pomijane przy wyznaczaniu [inicjatora](./placeholders#requester). `skip` zastępuje wbudowaną listę, a `skip = []` oznacza, że nic nie jest pomijane. `extend_skip` dopisuje nazwy do `skip` lub do listy wbudowanej. Jeśli chcesz pomijać także edytor, w którego terminalu pracujesz:

```toml
[requester]
extend_skip = ["nvim"]
```

Wpis może być nazwą albo prefiksem zakończonym `*`, np. `tmux*`. Nazwa jest porównywana dokładnie, z rozróżnianiem wielkości liter, z `comm` procesu, a jeśli `comm` nie da się odczytać — z nazwą pliku wykonywalnego. Jądro obcina `comm` do 15 bajtów, więc dla dłuższych nazw użyj prefiksu. Błędny wpis jest zgłaszany razem z miejscem w pliku. `touchcue check` wypisuje obowiązującą listę; jeśli chcesz ją zmodyfikować, skopiuj do konfiguracji listę po `skip =`.

## IPC {#ipc}

| Sekcja | Klucz | Domyślnie |
| --- | --- | --- |
| `[ipc]` | `enabled` | `true` |
| `[dbus]` | `enabled` | `true` |
| `[compat.maxbaz_socket]` | `enabled` | `false` |

Wszystkie punkty końcowe wymagają `XDG_RUNTIME_DIR`. Działają tylko w jedną stronę: klienci niczego nie mogą zmienić.

- **Gniazdo zdarzeń** `$XDG_RUNTIME_DIR/touchcue/events.sock`, włączane w `[ipc]`. Każdy wiersz to jeden obiekt JSON z polami `kind` (`started`, `updated` lub `ended`), `id`, `state`, `reason` (tylko dla `ended`: `touched`, `cancelled`, `failed` lub `timed_out`), `source` i `values`. Nowy klient najpierw dostaje wiersz `started` dla każdego aktywnego żądania. Połączyć się mogą wyłącznie procesy tego samego użytkownika.
- **D-Bus**, włączany w `[dbus]`: nazwa na magistrali sesji `io.github.theaifam5.Touchcue`, obiekt `/io/github/theaifam5/Touchcue`, interfejs `io.github.theaifam5.Touchcue1`. Udostępnia właściwość `Active`, metodę `ActiveRequests` oraz sygnały `RequestStarted`, `RequestUpdated` i `RequestEnded`.
- **Gniazdo zgodności** `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, włączane w `[compat.maxbaz_socket]`. Wysyła 5-bajtowe komunikaty w formacie yubikey-touch-detector: `U2F_1`/`U2F_0` dla FIDO, `GPG_1`/`GPG_0` dla gpg i ssh oraz `MAC_1`/`MAC_0`. Uwzględniane są tylko żądania, które nadal czekają na dotknięcie.

`values` zawiera wyłącznie następujące symbole zastępcze: wszystkie klucze `request.*`, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` i `requester.label`. Ścieżki plików wykonywalnych, PID-y, UID-y i wiersze poleceń nigdy nie są publikowane. `process.chain` i `requester.*` ujawniają jednak każdemu klientowi gniazda i nazwy na magistrali nazwy procesów nadrzędnych klienta, np. powłoki i narzędzi, które go uruchomiły.

Jeśli gniazdo zdarzeń lub nazwa na magistrali są już zajęte, kolejne wywołanie `touchcue run` kończy działanie.
