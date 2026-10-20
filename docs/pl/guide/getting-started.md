# Pierwsze kroki

::: warning Wczesna faza rozwoju
touchcue działa tylko w Linuksie. Archiwa wydań dla Windows i macOS już są, ale na razie niczego nie wykrywają; szczegóły na stronie [Platformy](./platforms).
:::

## Do czego służy

Gdy sprzętowy uwierzytelniacz czeka na fizyczne dotknięcie, touchcue pokazuje okienko, które nie przejmuje fokusu, albo powiadomienie pulpitu. Monit podaje urządzenie oraz program, który poprosił o dotknięcie, na przykład przeglądarkę albo narzędzie, które wywołało `git`. touchcue wykrywa:

- klucze FIDO2/U2F dowolnego producenta, odczytując ich urządzenia hidraw;
- karty OpenPGP obsługiwane przez gpg-agent, używane przez `gpg` oraz przez ssh dzięki obsłudze ssh w gpg-agent. Wymaga to polecenia [`touchcue gpg install`](./gpg).

## Instalacja

### Z archiwum wydania

Każde [wydanie](https://github.com/theaifam5/touchcue/releases) zawiera osobne archiwum `touchcue-<target>.tar.gz` dla każdej platformy oraz plik `SHA256SUMS`. Na przykład dla Linuksa x86_64 z glibc:

```sh
curl -LO https://github.com/theaifam5/touchcue/releases/latest/download/touchcue-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/theaifam5/touchcue/releases/latest/download/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf touchcue-x86_64-unknown-linux-gnu.tar.gz
cd touchcue-x86_64-unknown-linux-gnu
```

Archiwa dla Linuksa są dostępne dla `x86_64` i `aarch64`, każde w wersji `gnu` i `musl`. W archiwum znajdziesz `touchcue`, `touchcue-helper`, `completions/`, `man/touchcue.1`, licencje i README. Zainstaluj plik wykonywalny, stronę podręcznika oraz uzupełnianie dla swojej powłoki:

```sh
install -D touchcue ~/.local/bin/touchcue
install -Dm644 man/touchcue.1 ~/.local/share/man/man1/touchcue.1
install -Dm644 completions/touchcue.bash ~/.local/share/bash-completion/completions/touchcue
install -Dm644 completions/touchcue.fish ~/.config/fish/completions/touchcue.fish
```

W `completions/` są też `touchcue.zsh`, `touchcue.nu` i `touchcue.ps1`. Uzupełnianie wymaga narzędzia [`usage`](https://usage.jdx.dev/cli/completions) w `PATH`, a w przypadku basha także wcześniej załadowanego bash-completion. `touchcue-helper` na razie nic nie robi, więc nie musisz go instalować.

### Przez mise

```sh
mise use -g packslip:github.com/theaifam5/touchcue
```

Jeśli w konfiguracji mise ustawiono `minimum_release_age`, wydania młodsze niż podany wiek mogą być niewidoczne.

### Ze źródeł

Zainstaluj toolchain Rusta w wersji przypiętej w `rust-toolchain.toml`, a potem uruchom w katalogu repozytorium:

```sh
cargo install --locked --path crates/touchcue
```

Polecenie instaluje `touchcue` w `~/.cargo/bin`. Osoby rozwijające projekt mogą zamiast tego użyć `mise run build`, które buduje wszystkie crate'y workspace'u do katalogu `target/`. Katalogi `completions/` i `man/` w repozytorium zawierają te same pliki co archiwa.

## Sprawdzenie instalacji

```sh
touchcue check
```

Polecenie wypisuje:

- czy konfiguracja jest poprawna;
- czy dostępne są kompozytor Waylanda, layer-shell Waylanda i usługa powiadomień;
- wszystkie urządzenia FIDO wraz z informacją, czy touchcue może je odczytać;
- backend wyjścia, który wybrałby `touchcue run`;
- punkty końcowe IPC i konfigurację gpg;
- liczbę hooków, w tym liczbę tych z `until`;
- listę procesów pomijanych przy wyznaczaniu inicjatora;
- [motyw ikon](./configuration#icons) i jego źródło.

Wiersze `requester` pokazują, jak touchcue widzi terminal, w którym go uruchomiono; pominięte procesy są ujęte w nawiasy kwadratowe:

```text
requester: skip = ["sh", "bash", …, "systemd", "init"] (defaults)
requester: here: touchcue ← [nu] ← [herdr] ← [herdr] ← [nu] ← kitty → "touchcue in kitty"
icons: theme = "Papirus-Dark" (portal org.gnome.desktop.interface)
```

Jeśli konfiguracja jest niepoprawna lub któregoś urządzenia FIDO nie da się odczytać, polecenie kończy się ze statusem 1.

`touchcue list-devices` wyświetla podłączone urządzenia FIDO. `touchcue trace` wypisuje sygnały wykrywania, dopóki go nie przerwiesz; przydaje się, gdy dotknięcie nie jest wykrywane.

## Uruchomienie demona

Na pierwszym planie:

```sh
touchcue run -v
```

`-v` włącza logi na poziomie info, a `-vv` na poziomie debug. Zmienna środowiskowa `TOUCHCUE_LOG` przyjmuje filtr `tracing` i ma pierwszeństwo przed obiema opcjami. Logi i `touchcue check` są kolorowane tylko w terminalu; kolory wyłączysz, ustawiając `NO_COLOR` na dowolną niepustą wartość.

### Jako usługa użytkownika systemd {#as-a-systemd-user-service}

Do każdego wydania dołączona jest też jednostka systemd `touchcue.service`:

```sh
mkdir -p ~/.config/systemd/user
curl -Lo ~/.config/systemd/user/touchcue.service \
  https://github.com/theaifam5/touchcue/releases/latest/download/touchcue.service
```

Jednostka uruchamia `touchcue run`, a systemd szuka tego pliku tylko w `/usr/local/bin` i `/usr/bin`. Jeśli touchcue jest zainstalowany gdzie indziej, nadpisz polecenie przez `systemctl --user edit touchcue.service`, na przykład dla `~/.local/bin`:

```ini
[Service]
ExecStart=
ExecStart=%h/.local/bin/touchcue run
```

Po instalacji przez `cargo install` użyj `%h/.cargo/bin/touchcue`. W przypadku mise ścieżkę poda `mise which touchcue`; zmienia się ona przy każdej aktualizacji. Na koniec uruchom usługę:

```sh
systemctl --user daemon-reload
systemctl --user enable --now touchcue.service
```

Jednostka startuje razem z `graphical-session.target`, więc potrzebny jest menedżer sesji, który aktywuje ten target.

## gpg i ssh przez gpg-agent

```sh
touchcue gpg install
```

Od tej chwili gpg-agent uruchamia touchcue jako swój `scdaemon-program`. Szczegóły na stronie [gpg i ssh](./gpg). Dla kluczy `ed25519-sk` i `ecdsa-sk` monit pokazuje już źródło FIDO, a [ssh askpass](./ssh-askpass) uzupełnia go o klucz i zdalnego użytkownika.

## Przejście z yubikey-touch-detector

1. Zatrzymaj i wyłącz jego gniazdo i usługę: `systemctl --user disable --now yubikey-touch-detector.socket yubikey-touch-detector.service`. Jeśli jednostka gniazda zostanie aktywna, nadal zajmuje ścieżkę gniazda, a touchcue nie uruchomi wtedy swojego gniazda zgodności.
2. Uruchom touchcue w sposób opisany powyżej.
3. Jeśli pasek stanu lub skrypt odczytuje `$XDG_RUNTIME_DIR/yubikey-touch-detector.socket`, włącz gniazdo zgodności:

   ```toml
   [compat.maxbaz_socket]
   enabled = true
   ```

touchcue udostępnia to gniazdo pod tą samą ścieżką i wysyła te same 5-bajtowe komunikaty: `U2F_1`/`U2F_0`, `GPG_1`/`GPG_0` oraz `MAC_1`/`MAC_0`. Dopóki gniazdo obsługuje jeszcze yubikey-touch-detector, touchcue je pomija i wyświetla ostrzeżenie. Więcej w sekcji [IPC](./configuration#ipc).

## Co dalej

- [Konfiguracja](./configuration)
- [Symbole zastępcze](./placeholders)
- [Dokumentacja CLI](/pl/reference/cli/)
