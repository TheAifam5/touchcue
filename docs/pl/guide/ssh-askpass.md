# Klucze bezpieczeństwa OpenSSH

::: v-pre
Dla kluczy `ed25519-sk` i `ecdsa-sk` okienko z prośbą o dotknięcie wyświetla już źródło FIDO. `touchcue askpass` dodaje do niego informację, który klucz jest używany, a gdy podpisuje ssh-agent — także dla którego zdalnego użytkownika. W szablonach ten tekst jest dostępny jako `{{ request.detail }}`, np. `ED25519-SK SHA256:… → user git`. OpenSSH nie podaje hosta docelowego.
:::

`touchcue askpass` nigdy nie tworzy własnego okienka. Jedynie uzupełnia szczegóły w okienku FIDO dla tego samego dotknięcia.

## Jak to działa

Gdy OpenSSH potrzebuje potwierdzenia obecności użytkownika, uruchamia program wskazany w `SSH_ASKPASS` z odpowiednim komunikatem, a po zakończeniu sprawdzania wysyła mu `SIGTERM`, niezależnie od tego, czy klucz został dotknięty. `touchcue askpass` przekazuje komunikat demonowi i czeka na ten sygnał.

OpenSSH uruchamia askpass dla takiego komunikatu tylko wtedy, gdy ustawiono `DISPLAY` lub `WAYLAND_DISPLAY` albo `SSH_ASKPASS_REQUIRE=force`. Zwykłe `ssh`, którego standardowe wyjście błędów jest terminalem, wypisuje komunikat w terminalu i nie uruchamia askpass.

Jeśli klucz jest przechowywany w ssh-agent, komunikat wysyła agent, więc opisane niżej zmienne muszą być ustawione w jego środowisku. Jeśli agent działa jako usługa użytkownika systemd, użyj `systemctl --user set-environment` lub pliku drop-in z `Environment=`.

## Konfiguracja

`SSH_ASKPASS` przyjmuje ścieżkę programu bez argumentów. touchcue działa jako askpass, jeśli zostanie uruchomiony przez dowiązanie o nazwie `touchcue-askpass`:

```sh
ln -s "$(command -v touchcue)" ~/.local/bin/touchcue-askpass
```

Następnie ustaw w środowisku ssh lub ssh-agent:

```sh
export SSH_ASKPASS="$HOME/.local/bin/touchcue-askpass"
export SSH_ASKPASS_REQUIRE=prefer   # or "force" when DISPLAY/WAYLAND_DISPLAY is not set
export TOUCHCUE_ASKPASS_FALLBACK=/usr/lib/ssh/ssh-askpass
```

`touchcue askpass <message>` działa tak samo i przydaje się do testów; wszystkie argumenty po `askpass` są przekazywane dalej bez analizowania.

W demonie musi być włączone `[sources.gpg]` (to ustawienie domyślne), bo zgłoszenia askpass przechodzą przez jego gniazdo pomocnicze.

## Hasła

touchcue nigdy nie odczytuje haseł. Wszystkie pozostałe monity askpass, czyli prośby o hasło i monity `confirm`, są przekazywane bez zmian do programu wskazanego w `TOUCHCUE_ASKPASS_FALLBACK`, czyli do askpass, którego zwykle używasz, np. `ssh-askpass`, `ksshaskpass` lub `lxqt-openssh-askpass`. Jeśli ta zmienna nie jest ustawiona, `touchcue askpass` wypisuje błąd i kończy się ze statusem 1, co OpenSSH traktuje jak anulowanie monitu. Program zapasowy nie może prowadzić z powrotem do touchcue.

## Bez demona

Jeśli demon nie działa, `touchcue askpass` i tak czeka na sygnał od OpenSSH i kończy się bez żadnego komunikatu, więc nigdy nie blokuje ssh. Jeśli przez 600 sekund nie dostanie sygnału, kończy się samodzielnie.
