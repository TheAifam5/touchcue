# Launchery

touchcue może wyświetlać monit w wybranych przez ciebie programach, takich jak rofi, fuzzel, `hyprctl notify` czy skrypt paska stanu. touchcue nie zawiera kodu dla żadnego z nich. Zamiast tego [hook z `until = "ended"`](./hooks#hooks-that-last-for-a-request) uruchamia program na czas trwania żądania i przekazuje mu monit na dwa sposoby:

- w zmiennych `TOUCHCUE_TITLE` i `TOUCHCUE_BODY`, czyli tekst wyświetlany w powiadomieniu, łącznie z `(cancelled)` lub `(timed out)` w [języku](./configuration#locale) monitu, dopóki monit jeszcze widnieje po zakończeniu operacji;
- na stdin jako wiersz JSON, gdy monit się pojawia i gdy znika, a przy `on_change = "stream"` także przy każdej zmianie pomiędzy tymi chwilami.

Działa to w Linuksie.

```toml
[output]
mode = "none"

[[hooks]]
on = ["started"]
until = "ended"
command = ["sh", "-c", 'exec rofi -e "$TOUCHCUE_BODY"']
stop_signal = "SIGINT"
```

`mode = "none"` wyłącza okienko, więc jego miejsce zajmuje launcher. Jeśli chcesz widzieć oba, pomiń sekcję `[output]`.

## Przykłady

Do każdego przykładu dołączono fragment konfiguracji i uwagi:

- [rofi](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/rofi): okno komunikatu dla każdego żądania.
- [fuzzel](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/fuzzel): puste menu z monitem.
- [hyprctl](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/hyprctl): powiadomienie Hyprlanda.
- [waybar](https://github.com/TheAifam5/touchcue/tree/main/examples/launchers/waybar): moduł paska stanu, który zlicza oczekujące żądania.

## Wybór `on_change`

- Programy wyświetlające stały tekst, takie jak rofi czy fuzzel, korzystają z domyślnego `restart`: gdy zmieni się któraś wartość żądania, touchcue zatrzymuje program i uruchamia go ponownie z nowym tekstem.
- Programy czytające stdin, takie jak skrypt dla waybara, korzystają ze `stream` i dostają każdą zmianę jako wiersz `update`.
- Programy, które mają się pojawić tylko raz na żądanie, korzystają z `ignore`.

Program, który zakończy działanie przed końcem swojego żądania, np. menu zamknięte klawiszem Escape albo `hyprctl notify`, które kończy się natychmiast, zostaje uznany za odrzucony: touchcue nie uruchomi go ponownie, dopóki nie zmieni się któraś wartość żądania.

## Niezaufany tekst

Nazwy aplikacji i procesów, a co za tym idzie także tytuł i treść, pochodzą od procesów, które wysłały żądanie.

- `command` jest uruchamiane dokładnie tak, jak je zapisano; touchcue nigdy nie wstawia do niego wartości. Powłokę uruchamiaj tylko jawnie, ujmuj zmienne w cudzysłowy i nigdy nie wklejaj zmiennej bezpośrednio do treści skryptu.
- Wartość może zaczynać się od `-`. Przekaż ją po `--`, dołącz do opcji, jak w `--prompt-only="$TOUCHCUE_BODY"`, albo poprzedź stałym tekstem, tak jak w przykładzie z hyprctl.
- Programy, które interpretują znaczniki Pango lub HTML, wymagają zabezpieczenia znaków specjalnych w tekście. Przykład z waybarem to robi, a przykład z rofi w ogóle nie włącza obsługi znaczników.
