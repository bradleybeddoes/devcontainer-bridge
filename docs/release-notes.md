# v0.4.1

## Finder image files on macOS

- `dbr paste` now transfers the contents of one local PNG or JPEG file copied in Finder instead of capturing Finder's rendered file icon.
- Unreadable, oversized, unsupported, and multiple file selections fail rather than falling back to an icon.
- Image data copied from applications such as Preview continues to use the existing PNG/JPEG and TIFF conversion paths.

## Upgrade

Only the macOS host binary must be upgraded for this fix; the clipboard protocol is unchanged and v0.4.0 container clients remain compatible. Install v0.4.1 on the Mac and restart the running host daemon.

Clipboard sharing remains opt-in and authenticated. A client with the host token can request supported clipboard contents without per-request confirmation. Transfers remain limited to 20 MiB and use the bridge's existing unencrypted transport; keep its ports on the trusted host/container network.

Written by Codex; exact model unavailable.
