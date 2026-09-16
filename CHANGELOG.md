# Changelog

Notable changes to `devcontainer-bridge` are documented here.

## [0.4.2] - 2026-09-16

### Fixed

- Host upgrades now replace the installed executable atomically instead of overwriting signed code in place, preventing macOS from killing the new binary because of stale kernel code-signature state.

## [0.4.1] - 2026-09-16

### Fixed

- On macOS, `dbr paste` now reads the contents of one local PNG or JPEG file copied in Finder instead of capturing Finder's rendered file icon. Unreadable, oversized, unsupported, and multiple file selections fail rather than falling back to the icon.
- Reverse Unix-socket connections are no longer dropped when the data-channel handshake arrives just before its control-channel request is registered.

## [0.4.0] - 2026-09-15

### Added

- Added opt-in, authenticated PNG/JPEG clipboard transfer from the host with `dbr paste`, including native macOS capture, TIFF-to-PNG conversion, and Wayland/X11 support on Linux.
- Added private local image storage and optional `--tmux-target` path insertion without submitting input.
- Added persistent `[clipboard] enabled` host configuration plus one-run `--allow-clipboard` and `--no-clipboard` overrides.

### Changed

- Invalid host configuration now stops daemon startup instead of silently using defaults.

[0.4.2]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/bradleybeddoes/devcontainer-bridge/releases/tag/v0.4.0

Written by Codex; exact model unavailable.
