# Changelog

Notable changes to `devcontainer-bridge` are documented here.

## [Unreleased]

## [0.5.1] - 2026-09-20

### Fixed

- The devcontainer feature (**0.3.1**) now restarts the container daemon if it exits unexpectedly. The entrypoint previously started it once at container boot with no supervision, so a crashed daemon left the container silently disconnected from the host until it was rebuilt. A clean exit, such as SIGTERM during container shutdown, still ends the loop.
- A container daemon whose authentication token the host rejects now exits instead of being restarted every five seconds forever. Retrying cannot fix a wrong token, and each attempt registered again with the host. Requires both the v0.5.1 binary and feature **0.3.1**.
- The container daemon no longer re-resolves a process name for every listening port on every scan. It only resolved names the first time a port was forwarded and discarded the rest, and ports owned by another user could never resolve at all, so an idle container with four forwarded ports spent 4.2% of a CPU core doing work it threw away. See [docs/performance.md](docs/performance.md).
- Opening a URL no longer holds the browser lock while the browser runs, so a slow browser can no longer block port forwarding for every connected container. The wait is now bounded at ten seconds, and a browser that outlasts it is terminated rather than left running.

### Changed

- Periodic tasks no longer fire catch-up ticks back to back after a cycle overruns its interval, and the host's Unix-socket scan now runs off the async runtime's worker threads.

## [0.5.0] - 2026-09-18

### Added

- `dbr paste` now transfers every image in a macOS Finder selection instead of refusing multi-file copies. Images are saved side by side as `clipboard-1`, `clipboard-2`, and so on in one transfer directory, every path is printed, and `--tmux-target` inserts all of them. A selection is limited to 16 images and 64 MiB, and is transferred whole or not at all: any file that is not a PNG or JPEG matching the requested format fails the request.

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

[0.5.1]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/bradleybeddoes/devcontainer-bridge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/bradleybeddoes/devcontainer-bridge/releases/tag/v0.4.0

Written by Codex; exact model unavailable. Multi-image clipboard entry written by Claude (claude-opus-5). 0.5.1 entries written by Claude (claude-opus-5).
