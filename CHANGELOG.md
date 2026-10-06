# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
