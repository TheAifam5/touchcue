# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2](https://github.com/TheAifam5/touchcue/compare/v0.2.1...v0.2.2) - 2026-10-08

### Fixed

- *(ui)* read Wayland outputs before the first popup

### Other

- update Cargo.lock dependencies
- release v0.2.1
- *(ui)* add an example that shows the screenshot scenarios

## [0.2.1](https://github.com/TheAifam5/touchcue/compare/v0.2.0...v0.2.1) - 2026-10-08

### Fixed

- *(ui)* read Wayland outputs before the first popup

### Other

- *(ui)* add an example that shows the screenshot scenarios

## [0.2.0](https://github.com/TheAifam5/touchcue/compare/v0.1.1...v0.2.0) - 2026-10-08

### Added

- *(cli)* render prompts off the async runtime
- *(cli)* use the detected icon theme for app and rule icons
- *(cli)* run hooks that last for a request
- *(core)* name the process that requested the touch
- *(core)* render templates with MiniJinja
- *(core)* configure rule icons and the icon theme
- *(core)* configure hooks that last for a request
- *(appinfo)* look up icons in the user's icon theme
- *(ipc)* read desktop settings from the portal
- *(hooks)* keep a command running while a request waits

### Fixed

- *(ipc)* log expected portal misses at debug

### Other

- *(ui)* use the shared outcome suffix
- point the install steps at v0.1.1 and fix stale text
- *(ipc)* list the requester and chain placeholders as published
- *(hooks)* treat ESRCH as an ended process

## [0.1.1](https://github.com/TheAifam5/touchcue/compare/v0.1.0...v0.1.1) - 2026-10-06

### Added

- *(hooks)* run commands on daemon events

### Fixed

- *(appinfo)* skip hidden desktop entries when matching executables
- *(packaging)* allow netlink in the systemd unit, so gpg requests name the application; replace the v0.1.0 unit

## [0.1.0](https://github.com/TheAifam5/touchcue/releases/tag/v0.1.0) - 2026-10-06

### Added

- *(cli)* add the daemon, gpg setup and askpass
- *(ui)* centre popups, choose outputs and block clicks while waiting
- *(ui)* add Wayland and X11 popups and notification backend
- *(ipc)* add event socket, D-Bus service and reporter listener
- *(config)* add popup output and modal settings
- *(core)* add the request model, state machine and configuration
- *(detect)* watch FIDO hidraw devices on Linux
- *(appinfo)* attribute devices and sockets to applications on Linux
