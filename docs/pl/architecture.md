# Architektura

## Crate'y

| Crate | Zakres odpowiedzialności |
| --- | --- |
| `touchcue` | Plik wykonywalny `touchcue`: CLI, pętla demona, `check`, `gpg install`, wrapper scdaemon, `askpass` i wrapper pinentry. |
| `touchcue-core` | Model domenowy, maszyna stanów żądań, parsowanie CTAPHID i Assuan, szablony monitów (MiniJinja) oraz konfiguracja. Nie zależy od systemu operacyjnego. |
| `touchcue-detect` | Źródła żądań dotknięcia: linuksowy obserwator hidraw dla kluczy FIDO. |
| `touchcue-appinfo` | Ustala dla identyfikatora procesu nazwę, identyfikator i ikonę aplikacji, a spośród jego przodków wskazuje inicjatora. Wyszukuje ikony w motywach ikon i odczytuje bieżący motyw z plików ustawień pulpitu. |
| `touchcue-ui` | Backendy okienek i powiadomień. |
| `touchcue-ipc` | Gniazdo zdarzeń JSON, gniazdo zgodności, usługa D-Bus, gniazdo pomocnicze, przez które zgłaszają się gpg, askpass i pinentry, zapytania do gpg-agent oraz ustawienia portalu pulpitu. |
| `touchcue-hooks` | Uruchamia polecenia z `[[hooks]]` przy zdarzeniach, a dla hooków z `until` jeden proces na żądanie. Polecenia działają bez powłoki, każde we własnej grupie procesów. |
| `touchcue-helper` | Opcjonalny pomocnik z podwyższonymi uprawnieniami. Na razie to zaślepka: nic nie jest jeszcze zaimplementowane. |

## Środowisko uruchomieniowe

`touchcue run` to jeden proces działający na wielowątkowym środowisku tokio z dwoma wątkami roboczymi. Obserwator hidraw, gniazdo pomocnicze, punkty końcowe IPC i interfejs użytkownika działają jako osobne zadania. Obserwator i gniazdo pomocnicze przesyłają sygnały kanałami o ograniczonej pojemności do jednej pętli demona. To ona zarządza maszyną stanów, wysyła monity do zadania interfejsu, a zdarzenia do zadań IPC i do hooków. Każdy hook ma własne zadanie z ograniczoną kolejką, które uruchamia jego polecenia. Hook z `until` ogranicza dodatkowo kolejkę stdin każdego ze swoich procesów, a takich procesów działa jednocześnie najwyżej 8. Pętla demona nigdy na nie nie czeka. Operacje blokujące, takie jak przeszukiwanie `/proc` w celu przypisania żądania czy wyszukiwanie ikony reguły, trafiają do puli blokującej tokio. Pula ma najwyżej cztery wątki, a na przypisanie przypada limit 1 s, na wyszukanie ikony zaś 0,5 s. Po SIGINT lub SIGTERM każde zadanie kończy się w ustalonym czasie.

Wrapper scdaemon, `touchcue askpass` i `touchcue pinentry` to osobne procesy touchcue, które uruchamiają gpg-agent i OpenSSH. Wysyłają zgłoszenia do demona przez `$XDG_RUNTIME_DIR/touchcue/helper.sock`, a demon nigdy na nie nie odpowiada.

## Cykl życia żądania

Każde żądanie zaczyna się w stanie `waiting` i kończy w dokładnie jednym ze stanów `touched`, `cancelled`, `failed` lub `timed_out`. Po zakończeniu operacji żądanie trwa jeszcze przez `sources.fido.retry_window_ms`. Jeśli w tym czasie klient ponowi próbę, to samo żądanie jest kontynuowane, a monit przez ten czas pokazuje wynik.

```
waiting -> touched | cancelled | failed | timed_out
```

## Wykrywanie FIDO

Uwierzytelniacz FIDO, który czeka na dotknięcie, wysyła ramki CTAPHID `KEEPALIVE` (`0xBB`) ze statusem `UPNEEDED` (`0x02`). touchcue śledzi je osobno dla każdego identyfikatora kanału CTAPHID. Żądanie kończy się wraz z ostatnią ramką z tym samym identyfikatorem kanału albo wtedy, gdy przez `sources.fido.keepalive_timeout_ms` (domyślnie 1,5 s) nie nadejdzie żaden keepalive. Na podstawie kodu statusu CTAP2 w ostatniej ramce touchcue odróżnia anulowanie (`0x2D`, `0x27`) i przekroczenie limitu czasu (`0x2F`, `0x3A`) od dotknięcia. W U2F odpowiedź ze słowem statusu `0x6985` (warunki niespełnione) oznacza, że klucz czeka na dotknięcie.

Ramki z innymi identyfikatorami kanałów CTAPHID nie kończą żądania ani nie zaczynają go od nowa.

## Wykrywanie OpenPGP

gpg-agent uruchamia touchcue jako swój `scdaemon-program`. Wrapper uruchamia właściwy scdaemon i obserwuje polecenia Assuan. Jeśli `PKSIGN`, `PKDECRYPT` lub `PKAUTH` nie dostanie odpowiedzi w ciągu 400 ms, wrapper zgłasza oczekiwanie na dotknięcie. Demon pokazuje je tylko wtedy, gdy polityka dotyku karty (UIF) dla danego klucza wymaga dotknięcia albo jest nieznana. Więcej na stronie [gpg i ssh](./guide/gpg).

## Przypisywanie żądań

Dla żądania FIDO demon szuka w `/proc` procesów, które mają otwarty węzeł urządzenia, a dla żądania OpenPGP — procesów połączonych z gniazdami gpg-agent. Pierwszy z nich staje się klientem (`process`). Wśród klientów gpg-agent pierwszeństwo mają narzędzia takie jak `gpg` i `ssh`, a po nich nowsze procesy; pewność wynosi wtedy `medium`. Jeśli `touchcue pinentry` zgłosił zatwierdzone okno PIN-u lub potwierdzenia, a klient gpg-agent tego okna jest jednym z połączonych procesów bieżącego użytkownika, to przy następnej operacji na karcie OpenPGP w ciągu 30 s klientem zostaje właśnie ten proces, z pewnością `high`. Anulowane lub nieudane okno niczego nie zmienia, a żądanie, które ma już przypisanie, zachowuje swojego klienta. Następnie demon przechodzi raz w górę po procesach nadrzędnych klienta (najwyżej 32) i ustala:

- aplikację (`app`): najbliższy proces należący do jednostki aplikacji systemd, a jeśli takiego nie ma, najbliższy proces, którego plik wykonywalny pasuje do pliku .desktop;
- inicjatora (`requester`): idąc od aplikacji w dół do klienta, pierwszy proces, którego nie ma na liście pomijanych powłok, multiplekserów, wrapperów i menedżerów usług, który nie uruchamia pliku wykonywalnego aplikacji i nie należy do innego użytkownika;
- łańcuch (`process.chain`): nazwy procesów od klienta w górę do aplikacji.

Przejście zostaje przerwane, jeśli czas uruchomienia procesu zmieni się między pierwszym sprawdzeniem a sprawdzeniem po odczytaniu jego nazw. Zasady i ich ograniczenia opisuje strona [Symbole zastępcze](./guide/placeholders#requester).
