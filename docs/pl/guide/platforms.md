# Platformy

touchcue działa wyłącznie w Linuksie. Wydania zawierają też archiwa dla Windows (`x86_64`) i macOS (`x86_64` i `aarch64`), a CI uruchamia testy na obu tych systemach, jednak touchcue nie ma tam żadnych źródeł ani backendów wyjścia i niczego nie wykrywa. Polecenia `run`, `list-devices`, `trace` i `gpg` wypisują wtedy `not supported on this platform yet` i kończą się ze statusem 2.

## Linux

- touchcue działa jako proces użytkownika: na pierwszym planie albo jako usługa użytkownika systemd. Zobacz [Pierwsze kroki](./getting-started#as-a-systemd-user-service).
- Wykrywa klucze FIDO, otwierając ich węzły `/dev/hidraw*` tylko do odczytu. Działa to równolegle z przeglądarkami i ssh i nie odbiera im raportów. Wymaga prawa odczytu tych węzłów. touchcue nie dostarcza reguł udev: dostęp muszą zapewniać istniejące reguły systemu. W większości dystrybucji z systemd nadają one zalogowanemu użytkownikowi dostęp do kluczy bezpieczeństwa FIDO przez znacznik `uaccess`. `touchcue check` pokazuje, czy każdy klucz da się odczytać.
- Wskazuje aplikację, która wysłała żądanie. W tym celu szuka w `/proc` procesów, które mają otwarte urządzenie, a następnie ustala ich aplikację na podstawie jednostki cgroup, a jeśli to się nie uda — na podstawie pliku wykonywalnego i odpowiadającego mu pliku .desktop. Ukryte wpisy, np. programy obsługujące adresy URL, są pomijane. Żądania OpenPGP są przypisywane klientowi gpg-agent, przy czym pierwszeństwo mają narzędzia gpg i ssh. Inicjatorem, np. programem, który wywołał `git`, jest pierwszy proces poniżej aplikacji, który nie jest powłoką, multiplekserem ani wrapperem.
- Wyświetla okienka przez layer-shell Waylanda lub X11 (także XWaylanda), a powiadomienia wysyła przez sesyjną magistralę D-Bus.

Planowany jest opcjonalny pomocnik z podwyższonymi uprawnieniami, `touchcue-helper`, który ma dokładnie przypisywać żądania od procesów roota, PIV i OATH oraz Ledgera. Plik wykonywalny już istnieje, ale na razie nic nie robi.

Planowane źródła: PIV, OATH, YubiKey HMAC/OTP, fprintd, Trezor i Ledger.

## Windows

W planach.

- Dostęp do FIDO HID będzie prawdopodobnie wymagał usługi działającej jako SYSTEM.
- Pozostałe kwestie są jeszcze badane.

## macOS

W planach.

- Planowana jest podpisana aplikacja `.app` z LaunchAgentem.
- Może być potrzebne uprawnienie Monitorowanie wprowadzania (Input Monitoring). Kwestia jest jeszcze badana.
