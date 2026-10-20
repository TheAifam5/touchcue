---
layout: home

hero:
  name: touchcue
  text: Zeigt, wer darauf wartet, dass Sie Ihren Sicherheitsschlüssel berühren
  tagline: Noch in früher Entwicklung. Läuft unter Linux mit FIDO2/U2F-Schlüsseln sowie mit OpenPGP-Karten über gpg-agent.
  image:
    src: /screenshots/hero.webp
    alt: Zwei übereinander gestapelte touchcue-Popups – ein Browser wartet auf einen Passkey, claude im Terminal auf eine GPG-Signatur; die Ladekreisel werden nach der Berührung zu einem Häkchen und nach Ablauf der Zeit zu einer Uhr
  actions:
    - theme: brand
      text: Erste Schritte
      link: /de/guide/getting-started
    - theme: alt
      text: Auf GitHub ansehen
      link: https://github.com/theaifam5/touchcue

features:
  - title: Nennt den Urheber
    details: Die Aufforderung nennt das Programm, von dem die Anfrage stammt – also etwa das Tool, das git aufgerufen hat, und nicht git selbst. Dazu zeigt sie das Symbol der zugehörigen Anwendung und das wartende Gerät.
  - title: Kein Fokusdiebstahl
    details: Ein Popup unter Wayland (layer-shell) oder X11, das keinen Eingabefokus an sich zieht. Wo das nicht geht, springen Desktop-Benachrichtigungen ein.
  - title: Dort, wo Sie hinschauen
    details: Standardmäßig zentriert auf dem fokussierten Monitor – wahlweise auch auf allen Monitoren, auf dem unter dem Mauszeiger oder auf ausgewählten. Optional dunkelt eine modale Ebene den Bildschirm ab und fängt Klicks ab.
  - title: Stabile Aufforderungen
    details: touchcue verfolgt FIDO-Anfragen getrennt nach CTAPHID-Kanal-ID. Eine Aufforderung bleibt deshalb so lange stehen, bis genau ihre Anfrage endet.
  - title: Für Skripte und Statusleisten
    details: Ein JSON-Ereignis-Socket, ein D-Bus-Dienst und das Socket-Protokoll von yubikey-touch-detector.
  - title: Hooks
    details: Führen Sie bei Ereignissen eigene Befehle aus – etwa einen Ton beim Start einer Anfrage – oder lassen Sie sie für die Dauer einer Anfrage laufen, um die Aufforderung in rofi, fuzzel oder einer Statusleiste anzuzeigen. Das geschieht ohne Shell; Werte kommen ausschließlich über Umgebungsvariablen und stdin an.
---
