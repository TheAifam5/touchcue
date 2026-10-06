---
layout: home

hero:
  name: touchcue
  text: Shows who is waiting for your security key touch
  tagline: Early development. Works on Linux for FIDO2/U2F keys and OpenPGP cards through gpg-agent; not released yet.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/theaifam5/touchcue

features:
  - title: Names the requester
    details: The prompt shows the requesting application's name and icon and the device that is waiting.
  - title: Does not steal focus
    details: A popup on Wayland (layer-shell) or X11 that takes no input focus, with desktop notifications as the fallback.
  - title: Stable prompts
    details: FIDO requests are tracked per CTAPHID channel ID, so a prompt stays until its own request ends.
  - title: For scripts and bars
    details: A JSON event socket, a D-Bus service, and the socket protocol of yubikey-touch-detector.
---
