# Unreleased

## Finder image files

- On macOS, `dbr paste` now transfers the contents of a single PNG or JPEG file copied in Finder instead of Finder's rendered file icon.
- Unsupported, oversized, unreadable, and multiple file selections fail instead of silently transferring an icon.

# v0.4.0

## Clipboard images

- Transfer host clipboard PNG and JPEG images into a devcontainer with `dbr paste`.
- Print a local image path or insert it into a tmux pane with `--tmux-target`, without submitting input. The integration works with any application that accepts file paths.
- Enable sharing persistently in the host’s `~/.config/dbr/config.toml`:

```toml
[clipboard]
enabled = true
```

Run `dbr restart` after changing the setting. `ensure` and subsequent restarts retain it. `host-daemon --no-clipboard` disables sharing for one run. Clipboard sharing requires authentication and remains disabled by default.

macOS supports native PNG/JPEG and TIFF-to-PNG conversion; Linux uses `wl-paste` or `xclip`. Transfers are limited to 20 MiB. Saved files remain until removed, and the existing bridge transport is unencrypted.

## Upgrade

Upgrade both host and container binaries. Follow the [permanent clipboard setup guide](https://github.com/bradleybeddoes/devcontainer-bridge/blob/v0.4.0/docs/clipboard.md) for installation, startup, container rebuilds, and nested tmux shortcuts. Host configuration errors now stop daemon startup instead of silently falling back to defaults.

Written by Codex; exact model unavailable.
