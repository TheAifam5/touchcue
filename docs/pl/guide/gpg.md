# gpg i ssh przez gpg-agent

touchcue może wyświetlać monit o dotknięcie przy operacjach na karcie OpenPGP wykonywanych przez gpg-agent: podpisywaniu i odszyfrowywaniu za pomocą `gpg` oraz uwierzytelnianiu ssh przez gpg-agent z obsługą ssh. W tym celu touchcue działa jako pośrednik między gpg-agent a scdaemon. Działa to tylko w Linuksie.

## Instalacja

```sh
touchcue gpg install
```

Polecenie:

1. ustala katalog domowy GnuPG za pomocą `gpgconf --list-dirs homedir`;
2. kopiuje `gpg-agent.conf` do `gpg-agent.conf.touchcue-backup`, a jeśli plik `gpg-agent.conf` nie istnieje, tworzy go z uprawnieniami 0600. Kopia zapasowa pozostała po wcześniejszej instalacji zostaje bez zmian i nigdy nie jest nadpisywana;
3. dopisuje wiersz `scdaemon-program <path to touchcue>`. Plik jest podmieniany atomowo: nowa zawartość trafia do pliku tymczasowego w tym samym katalogu, który następnie zastępuje oryginał przez zmianę nazwy. Jeśli `gpg-agent.conf` jest dowiązaniem symbolicznym, podmieniany jest jego cel, a samo dowiązanie zostaje. Plik zachowuje swoje uprawnienia;
4. uruchamia `gpgconf --reload gpg-agent` i `gpgconf --kill scdaemon`, aby przy następnym dostępie do karty uruchomił się touchcue.

Jeśli `gpg-agent.conf` już ustawia `scdaemon-program` na inny program, polecenie przerywa działanie i niczego nie zmienia.

Jeśli wiersz już jest w pliku, polecenie niczego nie zmienia. Poza plikiem tymczasowym nie odczytuje ani nie zapisuje żadnych innych plików w katalogu domowym GnuPG.

W konfiguracji zapisywana jest ścieżka pliku wykonywalnego touchcue, którym uruchomiono polecenie. Jeśli przeniesiesz lub zaktualizujesz ten plik, uruchom `touchcue gpg uninstall`, a potem `touchcue gpg install` z nowego pliku. `install` ostrzega, gdy plik wykonywalny leży w katalogu, który zostanie zastąpiony przy aktualizacji, np. w katalogu instalacji mise, w rejestrze cargo albo w dowolnym katalogu z numerem wersji w nazwie. Najlepiej wybierz stałą ścieżkę, np. `~/.cargo/bin` lub `/usr/bin`, albo dowiązanie do bieżącej wersji.

## Odinstalowanie

```sh
touchcue gpg uninstall
```

Polecenie usuwa wiersz `scdaemon-program` wskazujący touchcue, a następnie tak samo jak przy instalacji przeładowuje gpg-agent i zatrzymuje scdaemon. Kopia zapasowa zostaje zachowana. Wiersz wskazujący inny program nigdy nie jest usuwany.

`touchcue gpg uninstall --restore-backup` zamiast tego zastępuje `gpg-agent.conf` kopią zapasową, a potem ją usuwa. Jeśli `gpg-agent.conf` bez wiersza touchcue różni się od kopii zapasowej, polecenie przerywa działanie i niczego nie zmienia, ponieważ przywrócenie kopii usunęłoby te zmiany. W takim przypadku uruchom `touchcue gpg uninstall` i samodzielnie usuń kopię zapasową.

## Jak to działa

gpg-agent uruchamia swój `scdaemon-program` z argumentem `--multi-server`. Uruchomiony w ten sposób touchcue działa jako wrapper:

- Uruchamia właściwy scdaemon z katalogu `gpgconf --list-dirs libexecdir`, z tymi samymi argumentami i środowiskiem. Jeśli gpgconf nie da się uruchomić lub zwróci błąd, touchcue próbuje kolejno `/usr/libexec/scdaemon`, `/usr/lib/gnupg/scdaemon` i `/usr/lib/gnupg2/scdaemon`. Nigdy nie uruchamia programu, który w rzeczywistości jest samym touchcue.
- Kopiuje strumienie Assuan między gpg-agent a scdaemon bajt po bajcie, w obu kierunkach. stderr scdaemon, czyli log gpg-agent, przechodzi bez zmian. touchcue dopisuje tam jedynie własne, rzadkie ostrzeżenia z prefiksem `touchcue-scdaemon:`, np. gdy nie uda się skonfigurować pośrednika gniazda. Pod systemd trafiają one do dziennika `gpg-agent.service`; gpg-agent uruchomiony z `--daemon` odrzuca stderr scdaemon. Aby zmienić poziom logowania, ustaw `TOUCHCUE_LOG` w środowisku gpg-agent.
- Przekazuje do scdaemon sygnały SIGTERM, SIGINT, SIGHUP, SIGUSR1 i SIGUSR2 i kończy działanie z tym samym kodem wyjścia co scdaemon.
- Wyłącza u siebie możliwość zrzutu pamięci (non-dumpable), więc inne procesy tego samego użytkownika nie mogą odczytać jego pamięci. Jeśli się to nie uda, wrapper nie startuje: gpg-agent nie może wtedy korzystać z karty, a przyczynę widać w logu gpg-agenta. Zabezpiecza to tylko sam wrapper: PIN-y i dane przechodzą także przez gpg-agent i właściwy scdaemon.

Podczas kopiowania wrapper śledzi wiersze poleceń. `PKSIGN`, `PKDECRYPT` i `PKAUTH` rozpoczynają odpowiednio podpisywanie, odszyfrowywanie lub uwierzytelnianie. Jeśli scdaemon nie odpowie przez 400 ms, oznacza to, że karta czeka na dotknięcie. touchcue zgłasza to demonowi przez `$XDG_RUNTIME_DIR/touchcue/helper.sock`. Odpowiedź `OK` lub `ERR` od scdaemon kończy zgłoszenie.

Wpisywanie PIN-u nie jest traktowane jako oczekiwanie na dotknięcie. Gdy scdaemon prosi o PIN, odliczanie zatrzymuje się do odpowiedzi gpg-agent. Operacje, które kończą się w ciągu 400 ms, np. przy zapamiętanym dotknięciu, nie są zgłaszane.

Jeśli demon nie działa, wrapper jedynie przekazuje dane. Zgłaszanie nigdy nie opóźnia strumienia ani go nie zmienia.

## Polityka dotyku i przypisywanie

Demon pokazuje zgłoszoną operację tylko wtedy, gdy polityka dotyku karty (UIF) dla używanego slotu klucza wymaga dotknięcia albo gdy polityka jest nieznana. Odczytuje ją z gpg-agent poleceniami `KEYINFO --list` oraz `SCD GETATTR UIF-1` do `UIF-3`, i to wyłącznie wtedy, gdy nie trwa żadna operacja na karcie, ponieważ gpg-agent szereguje dostęp do karty. Do pierwszego odczytu pokazywana jest każda operacja. Jeśli demon nie znajdzie gniazd gpg-agent przez `gpgconf`, pokazuje każdą operację, ale żadnej nie przypisuje.

gpg-agent nie podaje, który klient zlecił operację. touchcue sprawdza więc procesy połączone z gniazdami gpg-agent (z pominięciem samego gpg-agent, scdaemon i touchcue) i wybiera najnowszy spośród `gpg`, `gpg2`, `gpgsm`, `ssh`, `scp`, `sftp`, `ssh-add` i `ssh-keygen`, a jeśli żadnego z nich nie ma — najnowszy inny proces. `request.confidence` ma wtedy wartość `medium`. Dzięki temu program, który tylko trzyma bezczynne połączenie, np. edytor, nie zostanie wskazany zamiast narzędzia, które faktycznie wysłało żądanie. Nadal jest to jednak tylko przypuszczenie. Ranking opiera się na nazwie procesu (`comm`), którą każdy proces tego samego użytkownika może zmienić, na przykład na `gpg`, a gdy jednocześnie połączone są dwa narzędzia, wskazane zostaje nowsze. Inicjator jest następnie wyznaczany od tego klienta, jak opisano na stronie [Symbole zastępcze](./placeholders#requester), a jego nazwa jest tak samo jedynie deklaracją. Operacja `auth` wykonywana, gdy klient jest połączony z gniazdem ssh gpg-agent, ma `request.source` równe `ssh`; pozostałe operacje mają `gpg`. Pośrednik yubikey-touch-detector zostaje wskazany jako klient (z pewnością `low`) tylko wtedy, gdy nie jest połączony żaden inny klient.

## Przypisywanie przez okno PIN-u {#pin-dialog-attribution}

W Linuksie touchcue może też działać jako wrapper pinentry dla gpg-agent. Utwórz dowiązanie:

```sh
ln -s "$(command -v touchcue)" ~/.local/bin/touchcue-pinentry
```

Dopisz ręcznie do `gpg-agent.conf` poniższy wiersz, zastępując `/home/USER` swoim katalogiem domowym:

```text
pinentry-program /home/USER/.local/bin/touchcue-pinentry
```

Przeładuj gpg-agent:

```sh
gpgconf --reload gpg-agent
```

Wrapper uruchamia `pinentry` znalezione w `PATH`. Aby użyć innego programu pinentry, ustaw w środowisku gpg-agent zmienną `TOUCHCUE_PINENTRY` na jego ścieżkę bezwzględną. Program, który w rzeczywistości jest samym touchcue, zostanie odrzucony. Wrapper ustawia `TOUCHCUE_PINENTRY_ACTIVE=1` dla właściwego pinentry i nie uruchomi się, jeśli ta zmienna jest już ustawiona. Wrapper można też uruchomić poleceniem `touchcue pinentry [ARGS]...`, które przekazuje argumenty dalej bez ich analizowania; pierwszym argumentem musi być `pinentry`.

Wrapper kopiuje stdin i stdout bajt po bajcie, dziedziczy stderr pinentry, przekazuje sygnały SIGTERM, SIGINT, SIGHUP, SIGUSR1 i SIGUSR2 i kończy działanie z kodem wyjścia pinentry. Sam nigdy nie odpowiada na polecenia Assuan. Wyłącza u siebie możliwość zrzutu pamięci, a gdy to się nie uda, nie startuje i okno PIN-u kończy się błędem; chroni to tylko wrapper, a nie gpg-agent ani właściwy pinentry.

Z kopii strumienia wrapper odczytuje wyłącznie `OPTION owner=<pid>/<uid>`, polecenia `GETPIN` i `CONFIRM` oraz rodzaj odpowiedzi: `OK` lub `ERR`. Opisy, monity, PIN-y i wiersze danych są przekazywane dalej, ale obserwator nigdy ich nie interpretuje, nie przechowuje ani nie zapisuje w logu. Wiersz od gpg-agent dłuższy niż 1002 bajty (łącznie ze znakiem końca wiersza) kończy obserwację, ale przekazywanie danych trwa dalej. Zgłoszenia zawierają tylko PID i UID właściciela, numer kolejny i wynik okna. Jeśli demon nie działa, wrapper jedynie przekazuje dane.

Jeśli w ciągu 30 s od zatwierdzenia okna PIN-u lub potwierdzenia nastąpi kolejna operacja na karcie OpenPGP, zostaje ona przypisana klientowi tego okna z `request.confidence` równym `high`, o ile klient jest nadal połączony z gpg-agent. Anulowane lub nieudane okno niczego nie przypisuje. Jeśli takiego okna nie było, np. gdy PIN jest zapamiętany, obowiązuje opisany wyżej ranking klientów.

Aby usunąć wrapper, usuń jego wiersz `pinentry-program` albo ręcznie przywróć poprzednie ustawienie pinentry, a potem przeładuj gpg-agent. `touchcue gpg uninstall` zmienia tylko ustawienie scdaemon.

## Gniazdo scdaemon

gpg-agent komunikuje się ze scdaemon na dwa sposoby: przez potok, z którym uruchomił scdaemon, oraz przez własne gniazdo scdaemon `S.scdaemon` w katalogu gniazd gpg (`gpgconf --list-dirs socketdir`). Z gniazda gpg-agent korzysta wtedy, gdy kartę potrzebuje inny klient, a potok jest zajęty.

Wrapper obsługuje również gniazdo. Gdy scdaemon po raz pierwszy odpowie przez potok, wrapper:

1. sprawdza, czy katalog gniazd należy do użytkownika i ma uprawnienia 0700, a następnie łączy się jednorazowo z `S.scdaemon`, aby upewnić się, że nasłuchuje tam scdaemon uruchomiony przez niego;
2. tworzy własne gniazdo `S.scdaemon.touchcue` z uprawnieniami 0600;
3. zamienia obie ścieżki jedną atomową zmianą nazwy, dzięki czemu z `S.scdaemon` można się łączyć przez cały czas, a gniazdo scdaemon trafia ostatecznie pod `S.scdaemon.touchcue`;
4. przyjmuje połączenia na `S.scdaemon` wyłącznie od tego samego użytkownika, najwyżej 32 jednocześnie, i przekazuje każde z nich bajt po bajcie do gniazda scdaemon, śledząc je tak samo jak potok.

Gdy scdaemon zakończy działanie, wrapper usuwa obie ścieżki, ale tylko wtedy, gdy każda z nich nadal wskazuje oczekiwane gniazdo; gniazdo nowszego scdaemon pozostaje nietknięte. Sprawdzenie i usunięcie to jednak osobne kroki, więc gniazdo utworzone dokładnie pomiędzy nimi mogłoby mimo to zostać usunięte. Jeśli pośrednik ulegnie awarii w trakcie działania scdaemon, wrapper przywraca pierwotne ścieżki.

Jeśli gniazda nie da się obsłużyć przez pośrednika, wrapper zapisuje w logu jedno ostrzeżenie, śledzi tylko potok i przekazuje wszystko jak dotąd. Dzieje się tak na przykład wtedy, gdy `S.scdaemon` nie istnieje, nie jest gniazdem tego samego użytkownika lub obsługuje je inny proces, albo gdy katalog gniazd jest współdzielony.
