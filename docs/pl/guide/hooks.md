# Hooki

Hook uruchamia wybrane przez ciebie polecenie, gdy w touchcue wystąpi określone zdarzenie, na przykład odtwarza dźwięk, gdy pojawia się żądanie. Hooki definiuje się jako wpisy `[[hooks]]` w [konfiguracji](./configuration#hooks). Działają w Linuksie.

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

`touchcue check` wypisuje liczbę skonfigurowanych hooków, w tym liczbę tych z `until`.

Hook z `until = "ended"` nie uruchamia polecenia przy każdym zdarzeniu, tylko jeden proces na każde żądanie, działający przez cały czas jego trwania. W ten sposób monit może wyświetlać launcher, taki jak rofi czy fuzzel; zobacz [Hooki działające przez całe żądanie](#hooks-that-last-for-a-request) i [Launchery](./launchers).

Jeśli używasz hooków, plik konfiguracyjny uruchamia polecenia z uprawnieniami twojego użytkownika. Zadbaj więc, aby tylko ty mógł go modyfikować, i nigdy nie uruchamiaj demona jako root z plikiem konfiguracyjnym, który może zmienić inny użytkownik.

## Zdarzenia

| Zdarzenie | Kiedy występuje |
| --- | --- |
| `started` | Rozpoczęło się żądanie. |
| `updated` | Żądanie się zmieniło, np. jego stan, procesy klienta lub szczegóły. |
| `ended` | Żądanie się zakończyło. |
| `waiting` | Żądanie się rozpoczęło lub zostało wznowione i czeka na dotknięcie. |
| `lingering` | Operacja żądania zakończyła się bez dotknięcia. Żądanie trwa jeszcze przez `retry_window_ms` na wypadek, gdyby klient ponowił próbę. |
| `revived` | Klient ponowił próbę dla żądania, które trwało jeszcze po zakończeniu operacji, i żądanie znów czeka. |
| `touched` | Klucz został dotknięty. |
| `cancelled` | Klient anulował operację. |
| `failed` | Operacja się nie powiodła. |
| `timed_out` | Klucz przestał zgłaszać żądanie albo upłynął limit czasu operacji. |
| `device_added` | touchcue zaczął obserwować klucz FIDO: po jego podłączeniu oraz dla każdego klucza podłączonego w chwili uruchomienia demona. |
| `device_removed` | touchcue przestał obserwować klucz FIDO, zwykle dlatego, że klucz odłączono. |
| `daemon_started` | Demon się uruchomił. |
| `daemon_stopping` | Demon się zatrzymuje. |

Jedna zmiana może wywołać kilka zdarzeń, zawsze w tej kolejności: `started`, `updated` lub `ended`, potem `revived`, `waiting` lub `lingering`, a na końcu `touched`, `cancelled`, `failed` lub `timed_out`. Na przykład nowe żądanie wywołuje najpierw `started`, a potem `waiting`.

`touched` występuje w chwili zakończenia żądania. `cancelled`, `failed` i `timed_out` występują od razu po zakończeniu operacji, razem z `lingering`, i nie są powtarzane przy `ended`. Anulowane żądanie, które zostanie wznowione, wraca do oczekiwania i może później zakończyć się zdarzeniem `touched`.

Zdarzenia urządzeń dotyczą wyłącznie kluczy FIDO i występują tylko wtedy, gdy włączone jest `[sources.fido]`. Zdarzenia `device_added` dla kluczy podłączonych przy starcie przychodzą po `daemon_started`. W rzadkich przypadkach, gdy jądro zgubi powiadomienia o urządzeniach, touchcue ponownie wywołuje `device_removed` i `device_added` dla każdego klucza.

`match` jedynie wybiera zdarzenia, które uruchamiają hook; niczego nie dowodzi. Nazwy aplikacji i procesów podają same procesy i nie są to uwierzytelnione tożsamości.

## Środowisko {#environment}

Polecenia dziedziczą środowisko touchcue, w tym `PATH`, `WAYLAND_DISPLAY` i `DBUS_SESSION_BUS_ADDRESS`, i dodatkowo dostają poniższe zmienne.

- `TOUCHCUE_EVENT` zawiera nazwę zdarzenia, np. `started`.
- Każdy [symbol zastępczy](./placeholders) publikowany przez touchcue trafia do zmiennej, której nazwa składa się z `TOUCHCUE_` i nazwy symbolu zapisanej wielkimi literami, z podkreśleniami zamiast kropek. Na przykład `app.name` to `TOUCHCUE_APP_NAME`, a `request.state` to `TOUCHCUE_REQUEST_STATE`.

Publikowane są te same symbole zastępcze co w [gnieździe zdarzeń](./configuration#ipc): wszystkie wartości `request.*`, `device.vendor`, `device.model`, `device.product`, `device.kind`, `device.transport`, `device.vid`, `device.pid`, `app.name`, `app.id`, `app.icon`, `app.container`, `process.name`, `process.chain`, `requester.name` i `requester.label`. Ścieżki plików wykonywalnych, PID-y, UID-y i wiersze poleceń nigdy nie są przekazywane.

Jeśli wartość jest nieznana, zmienna nie jest ustawiana. Znaki sterujące i niewidoczne są zastępowane spacjami, ciągi białych znaków są scalane w jedną spację, a wartość jest obcinana do 1024 znaków. Wartość może zaczynać się od `-`, więc jeśli przekazujesz ją programowi, który analizuje opcje, poprzedź ją `--`. Zdarzenia urządzeń ustawiają tylko zmienne `device.*`, a `daemon_started` i `daemon_stopping` — tylko `TOUCHCUE_EVENT`.

Zmienne z prefiksem `TOUCHCUE_`, które touchcue sam odziedziczył, np. `TOUCHCUE_LOG` lub `TOUCHCUE_ASKPASS_FALLBACK` z twojej powłoki, nie są przekazywane hookom. Każda zmienna `TOUCHCUE_` widoczna dla hooka pochodzi więc od touchcue.

## Uruchamianie

- **Bez powłoki.** `command` to program i jego argumenty, uruchamiane bezpośrednio. Znaki takie jak `$HOME`, `*`, `;` czy `|` są przekazywane dosłownie. Program jest wyszukiwany w `PATH`.
- **Wartości tylko w zmiennych.** Wartości symboli zastępczych nigdy nie trafiają do argumentów, więc spreparowana nazwa aplikacji nie może wstrzyknąć argumentów. Jeśli potrzebujesz powłoki, uruchom ją jawnie i ujmij zmienne w cudzysłowy, jak w przepisie z `notify-send` poniżej. Nigdy nie wklejaj zmiennej bezpośrednio do treści skryptu.
- **Limit czasu.** Jeśli polecenie działa dłużej niż `timeout_ms` (domyślnie 5 sekund), jego grupa procesów dostaje `SIGTERM`. Gdy polecenie się zakończy, grupa od razu dostaje `SIGKILL`, a jeśli się nie zakończy — sekundę później. touchcue zawsze czeka na zakończenie polecenia.
- **Sprzątanie procesów.** Po przekroczeniu limitu czasu lub przy zamykaniu demona kończone są tylko procesy, które nadal należą do grupy procesów hooka. Proces, który opuścił grupę, np. przez `setsid`, działa dalej. To samo dotyczy procesów uruchomionych w tle przez polecenie, które potem zakończyło się normalnie.
- **Wejście i wyjście.** stdin jest puste, z wyjątkiem [hooków z `until`](#standard-input). Pierwsze 4 KiB stdout i stderr trafia do logu na poziomie debug (`touchcue run -vv`), bez znaków sterujących. Wyjście hooków z `until` nigdy nie jest logowane, bo launcher może wypisywać to, co w nim wpisujesz. Po zakończeniu polecenia touchcue czyta jego wyjście jeszcze najwyżej przez 500 ms. Jeśli więc polecenie zostawia coś działającego w tle, przekieruj wyjście tego procesu, bo inaczej zostanie ono utracone.
- **Błędy.** Jeśli polecenia nie da się uruchomić, zakończy się ono niezerowym kodem wyjścia, przekroczy limit czasu albo zostanie zakończone przy zamykaniu demona, touchcue zapisuje ostrzeżenie z numerem hooka w konfiguracji i nazwą zdarzenia. Każdy hook ostrzega najwyżej raz na 10 sekund i zlicza wstrzymane ostrzeżenia; pozostałe trafiają do logu na poziomie debug.
- **Kolejność i limity.** Uruchomienia jednego hooka startują w kolejności zdarzeń, najwyżej `concurrency` naraz (domyślnie 4). Mogą jednak kończyć się w innej kolejności, niż wystartowały, więc jeśli efekty muszą następować jeden po drugim, ustaw `concurrency = 1`. Różne hooki działają niezależnie od siebie. Każdy hook ma kolejkę na 256 zdarzeń. Gdy jest pełna, najnowsze zdarzenie zostaje dla tego hooka odrzucone, zdarzenia w kolejce pozostają, a touchcue zapisuje ostrzeżenie z ograniczoną częstotliwością.
- **Zamykanie.** Gdy touchcue się zatrzymuje, wywołuje `daemon_stopping`, a potem czeka do 2 sekund na polecenia oczekujące w kolejce i działające, łącznie z poleceniami dla `daemon_stopping`, niezależnie od ich `timeout_ms`. Polecenia, które nadal działają, dostają wtedy `SIGTERM`, a następnie `SIGKILL`, tak jak przy przekroczeniu limitu czasu. Uruchomienia, które czekały w kolejce i nie wystartowały, zostają odrzucone.

touchcue nigdy nie blokuje się na hooku: okienka, powiadomienia i IPC działają normalnie, gdy polecenia są wykonywane.

## Hooki działające przez całe żądanie {#hooks-that-last-for-a-request}

Przy `until = "ended"` hook uruchamia dla każdego żądania jeden proces, który działa do zakończenia żądania. To dobre rozwiązanie dla programów, które same wyświetlają monit, np. okna komunikatu rofi:

```toml
[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

| Klucz | Wartości | Domyślnie |
| --- | --- | --- |
| `until` | `ended` | brak |
| `on_change` | `restart`, `stream`, `ignore` | `restart` |
| `stop_signal` | `SIGTERM`, `SIGINT`, `SIGHUP` | `SIGTERM` |
| `stop_grace_ms` | od 0 do 2000 | 1000 |
| `restart_interval_ms` | od 0 do 600000 | 200 |

`on`, `match` i `command` działają tak samo jak w pozostałych hookach, z następującymi ograniczeniami:

- `on` może zawierać tylko `started` i `waiting`; każde inne zdarzenie to błąd.
- `timeout_ms` i `concurrency` są niedozwolone: proces działa tak długo jak jego żądanie, a łącznie dla wszystkich hooków z `until` działa jednocześnie najwyżej 8 procesów. Proces ponad ten limit startuje, gdy tylko inny się zakończy, a touchcue zapisuje ostrzeżenie z ograniczoną częstotliwością.
- `on_change`, `stop_signal`, `stop_grace_ms` i `restart_interval_ms` bez `until` to błąd.

### Kiedy proces działa

- Proces startuje, gdy dla żądania pasującego do `match` wystąpi jedno ze zdarzeń z `on` — od razu, gdy monit żądania jest pokazywany. Pokazywany oznacza, że nie wycisza go żadna reguła, niezależnie od tego, co wyświetla `[output]`. Hook z `until` działa więc również przy `mode = "none"`.
- Dopóki [reguła](./configuration#rules) wycisza monit, proces jest zatrzymany, a wyciszone żądanie nie uruchamia nowego. Gdy monit znów jest pokazywany, startuje nowy proces.
- Proces jest zatrzymywany, gdy żądanie się kończy.
- `waiting` występuje także wtedy, gdy klient ponowi próbę i żądanie zostanie wznowione, więc hook ustawiony na `waiting` uruchomi ponownie proces, który wcześniej odrzucono.

`on_change` określa, co się dzieje, gdy zmieni się któraś wartość żądania:

- `restart` zatrzymuje proces i uruchamia nowy, gdy stary się zakończy, najwyżej raz na `restart_interval_ms`. Zmiany, które nastąpią w międzyczasie, są scalane, a nowy proces dostaje najnowsze wartości. Sama zmiana `request.elapsed` nie powoduje restartu.
- `stream` pozostawia proces i zapisuje każdą zmianę na jego stdin, także samą zmianę `request.elapsed`.
- `ignore` pozostawia proces i niczego mu nie przekazuje.

Proces, który zakończy się przed końcem swojego żądania (z dowolnym kodem wyjścia) albo nie zdoła się uruchomić, zostaje uznany za odrzucony; tak jest np. z menu zamkniętym klawiszem Escape. touchcue nie uruchomi go ponownie, dopóki nie zmieni się któraś wartość żądania, a przy `on_change = "ignore"` — dopóki znów nie wystąpi jedno ze zdarzeń z `on`. Zmiana, która nastąpiła przed zakończeniem procesu, jest uwzględniana: przy `restart` proces, który zakończy się, gdy restart czeka na upływ `restart_interval_ms`, zostanie uruchomiony ponownie z najnowszymi wartościami. Gdy proces zakończy się sam, touchcue wysyła też `SIGKILL` do jego grupy procesów. Programy, które zostawił w tle, kończą się więc razem z nim i nie omijają limitu 8 procesów.

### Środowisko procesu

Proces dostaje to samo [środowisko](#environment) co pozostałe hooki, z następującymi różnicami:

- `TOUCHCUE_EVENT` ma wartość `show` dla pierwszego procesu żądania, a `update` dla procesu uruchomionego ponownie po zmianie lub po wyciszeniu monitu.
- `TOUCHCUE_TITLE` i `TOUCHCUE_BODY` zawierają tytuł i treść monitu dokładnie w takiej postaci, w jakiej pokazuje je powiadomienie, czyli po zastosowaniu reguł, z tym samym czyszczeniem i limitem długości co pozostałe wartości. Jeśli żądanie trwa jeszcze po anulowaniu lub przekroczeniu limitu czasu, treść kończy się na `(cancelled)` lub `(timed out)` w [języku](./configuration#locale) monitu, tak jak w powiadomieniu. Skrypt, który sam formułuje opis wyniku, znajdzie stan w `TOUCHCUE_REQUEST_STATE`. Te wartości powstają ze skonfigurowanych szablonów, więc mogą zawierać niepublikowane symbole zastępcze, w tym wiersze poleceń.

Pozostałe hooki nie dostają `TOUCHCUE_TITLE` ani `TOUCHCUE_BODY`, ponieważ uruchamiają się także dla wyciszonych i zakończonych żądań, które nie mają monitu.

### Standardowe wejście {#standard-input}

stdin procesu to potok, w którym każdy wiersz zawiera jeden obiekt JSON:

```json
{"op":"show","id":3,"title":"Touch Yubico","body":"Firefox is waiting for a passkey","values":{"app.name":"Firefox","request.method":"fido2","request.state":"waiting"}}
{"op":"update","id":3,"title":"Touch Yubico","body":"Firefox is waiting for a passkey (cancelled)","values":{"app.name":"Firefox","request.method":"fido2","request.state":"cancelled"}}
{"op":"hide","id":3}
```

- `op` w pierwszym wierszu ma wartość `show` lub `update`, tak samo jak `TOUCHCUE_EVENT`. Przy `on_change = "stream"` każda późniejsza zmiana daje wiersz `update`, a `hide` pojawia się, gdy żądanie się zakończyło lub jego monit został wyciszony.
- `id` to identyfikator żądania (liczba).
- `title` i `body` to te same ciągi co w `TOUCHCUE_TITLE` i `TOUCHCUE_BODY`, łącznie z wynikiem, dopóki żądanie jeszcze trwa po zakończeniu operacji. Sam wynik podaje też `request.state` w `values`.
- `values` zawiera publikowane symbole zastępcze według nazw, oczyszczone tak samo jak w środowisku. Przykład pokazuje tylko część z nich.

touchcue nigdy nie czeka, aż proces odczyta dane. Każdy proces ma kolejkę na 64 wiersze. Gdy się zapełni, najstarszy oczekujący wiersz `update` zostaje odrzucony, a touchcue zapisuje ostrzeżenie z ograniczoną częstotliwością; wiersze `show` i `hide` są zawsze zachowywane. Proces, który zamknie stdin lub się zakończy, traci tylko swoje wiersze. Proces w trybie `stream`, który niczego nie czyta, choć odrzucono już 256 aktualizacji, zostaje zatrzymany, a touchcue zapisuje ostrzeżenie z ograniczoną częstotliwością. Jeśli od ostatniej przekazanej mu aktualizacji zmieniła się wartość inna niż `request.elapsed`, proces jest uruchamiany ponownie z najnowszymi wartościami, najwyżej raz na `restart_interval_ms`; w przeciwnym razie zostaje odrzucony.

### Zatrzymywanie procesu

Aby zatrzymać proces, touchcue zapisuje pozostałe wiersze i wiersz `hide`, zamyka stdin, a następnie wysyła `stop_signal` do grupy procesów. Na zapis przeznaczone jest najwyżej 200 ms. Proces, który w tym czasie nie odczyta danych, i tak dostaje sygnał, a ostatni wiersz na jego wejściu może być ucięty. Sygnał jest wysyłany natychmiast po zamknięciu stdin, więc program, który ma zareagować na wiersz `hide` lub na koniec wejścia, musi obsłużyć ten sygnał, np. go ignorować. Jeśli proces nie zakończy się w ciągu `stop_grace_ms`, grupa dostaje `SIGKILL`; touchcue zawsze czeka na zakończenie procesu.

Proces zatrzymywany w celu restartu nie dostaje wiersza `hide`.

Gdy touchcue się zatrzymuje, nie zapisuje już żadnych wierszy: procesy od razu dostają `stop_signal`, a po `stop_grace_ms` — `SIGKILL`. Wszystko to mieści się w 4 sekundach, które demon przy zamykaniu daje wszystkim hookom; jednostka systemd daje całemu demonowi 10 sekund na zatrzymanie.

## Przepisy

### Dźwięk przy nowym żądaniu

```toml
[[hooks]]
on = ["started"]
command = ["pw-play", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

Bez PipeWire użyj `paplay` z tym samym plikiem. Jeśli dźwięk ma być odtwarzany tylko dla niektórych żądań, dodaj `match`:

```toml
[[hooks]]
on = ["started"]
match = { "request.method" = "openpgp" }
command = ["paplay", "/usr/share/sounds/freedesktop/stereo/message.oga"]
```

### Powiadomienie po dotknięciu klucza

`notify-send` przyjmuje tekst w argumentach, dlatego odczytuje go ze środowiska powłoka:

```toml
[[hooks]]
on = ["touched"]
command = ["sh", "-c", 'notify-send -- "Security key touched" "${TOUCHCUE_APP_NAME:-An application}"']
```

Dzięki apostrofom ciąg TOML jest literałem, więc skrypt trafia do `sh` bez zmian. Cudzysłowy sprawiają, że nazwa aplikacji pozostaje jednym argumentem, a `--` zapobiega temu, by `notify-send` potraktował nazwę zaczynającą się od `-` jako opcję.

### Odświeżanie modułu waybar

Moduł waybar typu `custom` z `"signal": 8` ponownie uruchamia swoje `exec`, gdy waybar dostanie `SIGRTMIN+8`:

```toml
[[hooks]]
on = ["started", "ended"]
command = ["pkill", "-RTMIN+8", "waybar"]
concurrency = 1
```

Przy `concurrency = 1` sygnały są wysyłane jeden po drugim, w kolejności zdarzeń, więc ostatnie odświeżenie modułu następuje po ostatniej zmianie.

### Powiadomienie w Hyprlandzie

```toml
[[hooks]]
on = ["waiting"]
command = ["hyprctl", "notify", "1", "5000", "rgb(ff1ea3)", "Touch your security key"]
```

Kolejne argumenty to ikona (`1` oznacza informację), czas wyświetlania w milisekundach, kolor i treść komunikatu.
