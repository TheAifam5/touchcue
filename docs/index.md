---
layout: home

hero:
  name: touchcue
  text: Shows who is waiting for your security key touch
  tagline: Early development. v0.1.1 works on Linux for FIDO2/U2F keys and OpenPGP cards through gpg-agent.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/theaifam5/touchcue

features:
  - title: Names the requester
    details: The prompt names the program that asked, such as the tool that ran git rather than git itself, with its application's icon and the device that is waiting.
  - title: Does not steal focus
    details: A popup on Wayland (layer-shell) or X11 that takes no input focus, with desktop notifications as the fallback.
  - title: Where you look
    details: Centred by default, on the focused monitor, every monitor, the one under the cursor or the ones you name, with an optional modal dim that blocks clicks.
  - title: Stable prompts
    details: FIDO requests are tracked per CTAPHID channel ID, so a prompt stays until its own request ends.
  - title: For scripts and bars
    details: A JSON event socket, a D-Bus service, and the socket protocol of yubikey-touch-detector.
  - title: Hooks
    details: Run your own commands on events, such as playing a sound when a request starts, without a shell and with values passed only in environment variables.
---
