# v0.4.1

## Finder image files on macOS

- `dbr paste` now transfers the contents of one local PNG or JPEG file copied in Finder instead of capturing Finder's rendered file icon.
- Unreadable, oversized, unsupported, and multiple file selections fail rather than falling back to an icon.
- Image data copied from applications such as Preview continues to use the existing PNG/JPEG and TIFF conversion paths.

## Socket forwarding reliability

- Reverse Unix-socket connections no longer fail when the data connection reaches the host just before its control request is processed.

## Upgrade

The protocol is unchanged and v0.4.0 clients remain compatible. Upgrade the host binary to v0.4.1 and restart the running host daemon.

Clipboard sharing remains opt-in and authenticated. A client with the host token can request supported clipboard contents without per-request confirmation. Transfers remain limited to 20 MiB and use the bridge's existing unencrypted transport; keep its ports on the trusted host/container network.

Written by Codex; exact model unavailable.
