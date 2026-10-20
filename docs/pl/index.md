---
layout: home

hero:
  name: touchcue
  text: Pokazuje, kto czeka na dotknięcie klucza bezpieczeństwa
  tagline: Wczesna faza rozwoju. Działa w Linuksie z kluczami FIDO2/U2F oraz z kartami OpenPGP obsługiwanymi przez gpg-agent.
  image:
    src: /screenshots/hero.webp
    alt: Dwa okienka touchcue jedno nad drugim – przeglądarka czeka na klucz dostępu, a claude w terminalu na podpis GPG; wskaźniki oczekiwania zmieniają się w ptaszek po dotknięciu i w zegar po upływie czasu
  actions:
    - theme: brand
      text: Pierwsze kroki
      link: /pl/guide/getting-started
    - theme: alt
      text: Zobacz na GitHubie
      link: https://github.com/theaifam5/touchcue

features:
  - title: Wskazuje inicjatora
    details: Monit podaje program, który poprosił o dotknięcie (na przykład narzędzie, które wywołało git, a nie samego gita), razem z ikoną jego aplikacji i urządzeniem, które czeka na dotknięcie.
  - title: Nie zabiera fokusu
    details: Okienko w Waylandzie (layer-shell) lub X11 nie przejmuje fokusu. Gdy nie da się go pokazać, touchcue korzysta z powiadomień pulpitu.
  - title: Tam, gdzie patrzysz
    details: Domyślnie na środku ekranu, na aktywnym monitorze. Może też pojawiać się na wszystkich monitorach, na tym pod kursorem albo na wskazanych z nazwy, opcjonalnie z modalnym przyciemnieniem, które blokuje kliknięcia.
  - title: Stabilne monity
    details: Żądania FIDO są śledzone osobno dla każdego identyfikatora kanału CTAPHID, więc monit znika dopiero wtedy, gdy kończy się jego własne żądanie.
  - title: Dla skryptów i pasków
    details: Gniazdo zdarzeń JSON, usługa D-Bus oraz protokół gniazda znany z yubikey-touch-detector.
  - title: Hooki
    details: Uruchamiaj własne polecenia przy zdarzeniach, na przykład odtwarzaj dźwięk, gdy pojawia się żądanie, albo przez cały czas trwania żądania, żeby pokazać monit w rofi, fuzzelu lub pasku stanu. Polecenia działają bez powłoki, a wartości trafiają do nich wyłącznie przez zmienne środowiskowe i stdin.
---
