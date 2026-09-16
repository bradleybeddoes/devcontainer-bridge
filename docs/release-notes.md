# v0.4.2

## Reliable macOS upgrades

- The host installer now stages the downloaded executable in `/usr/local/bin` and atomically renames it into place.
- This avoids modifying signed Mach-O code in place, which can leave stale code-signature state in the macOS kernel and cause the upgraded binary to be killed on launch.
- Fresh installs and Linux installs retain their existing behavior.

## Upgrade

The protocol and application behavior are unchanged. Upgrade the macOS host with the v0.4.2 installer; v0.4.1 container clients remain compatible and do not need to be upgraded for this installer fix.

Written by Codex; exact model unavailable.
