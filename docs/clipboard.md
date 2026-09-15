# Paste host clipboard images into a devcontainer

`dbr paste` saves an image from the host clipboard as a local PNG or JPEG and prints its absolute path. It works with any application that accepts a file path. An optional tmux integration types that path into a chosen pane without pressing Enter.

## Enable on the host

Install a build with clipboard support on both the host and in the container. Older hosts do not understand clipboard requests.

Run the host daemon from your logged-in desktop session:

```sh
# If a host daemon is already running, stop it first.
dbr stop

dbr host-daemon --allow-clipboard
```

The second command stays in the foreground. Keep it running in a host terminal. Clipboard sharing is disabled by default; `dbr ensure` and `dbr restart` do not enable it. Stopping the daemon temporarily interrupts existing forwarded connections.

Authentication must remain enabled. The host uses its existing token, or generates one at `~/.config/dbr/auth-token` on first start. The container must have the same token through its existing token configuration or an explicit `--auth-token-file`. Combining `--allow-clipboard` with `--no-auth` is rejected.

`dbr paste` checks `--auth-token`, `--auth-token-file`, `DCBRIDGE_AUTH_TOKEN`, and `DCBRIDGE_AUTH_TOKEN_FILE` in that order, then `/run/secrets/dbr-auth-token` and `~/.config/dbr/auth-token`. An explicitly configured missing or invalid token is an error; it does not silently fall back.

macOS uses the built-in `osascript` and AppKit clipboard APIs. Linux requires `wl-paste` from `wl-clipboard` for Wayland or `xclip` for X11, with the desktop session's environment available to the daemon.

## Save an image

Copy an image on the host, then run inside the container:

```sh
dbr paste
```

Example output:

```text
/home/user/.cache/dbr/paste/image-a1b2c3/clipboard.png
```

The image must be on the clipboard as image data. Copying a file in Finder or a file manager may put only a file reference on the clipboard; open the image and copy its contents instead.

Options:

```sh
dbr paste --format png
dbr paste --format jpeg
dbr paste --output-dir ./images
dbr paste --auth-token-file /run/secrets/dbr-auth-token
dbr paste --host host.docker.internal --data-port 19286
```

| Format | Behavior |
| --- | --- |
| `auto` (default) | Prefer a native PNG representation, then native JPEG. On macOS, convert TIFF to PNG if neither is available. |
| `png` | Request PNG; macOS also supports TIFF-to-PNG conversion. |
| `jpeg` | Require a native JPEG representation. PNG and TIFF are not converted to JPEG. |

Native PNG and JPEG bytes are preserved. Images are limited to **20 MiB**; macOS TIFF conversion also limits dimensions to 40 million pixels. Other binary formats are not supported.

## Paste the path into tmux

Add this binding to `~/.tmux.conf` **inside the container**:

```tmux
bind-key V run-shell -b 'dbr paste --tmux-target "#{pane_id}" >/dev/null'
```

Reload the configuration from a container tmux pane:

```sh
tmux source-file ~/.tmux.conf
```

Copy an image on the host, focus the destination application, then press your tmux prefix followed by **Shift-V**. The binding captures the destination pane ID when invoked and runs the transfer in the background. Configure token access for the tmux server's environment, or add `--auth-token-file /path/to/token` to the binding.

### Nested tmux and shared dotfiles

With tmux on both the Mac and in the container, the binding belongs in the container. If both use Ctrl-a and the outer session binds Ctrl-a to `send-prefix`, press **Ctrl-a → Ctrl-a → Shift-V**. The doubled prefix passes one Ctrl-a through to the inner session.

If the same tmux configuration is used on the Mac and in Docker containers, guard the binding so the outer tmux does not intercept it:

```tmux
if-shell '[ -f /.dockerenv ] && dbr paste --help >/dev/null 2>&1' {
    bind-key V run-shell -b 'dbr paste --tmux-target "#{pane_id}" >/dev/null'
}
```

This binds the key only in Docker containers with a clipboard-capable `dbr` installed. It does not change Command-V text paste. Prefix + Shift-V has no default binding in tmux 3.3a; check your own configuration with `tmux list-keys -T prefix V` before adding it.

`--tmux-target` requires an exact pane ID such as `%3` and the inherited `TMUX` environment variable identifying its server. It inserts the saved path literally with shell quoting and does not submit the application's input. The receiving application decides how to use that path; dbr does not create application-specific attachments.

If insertion fails after the image was saved, the error reports the retained file's path.

## Files and privacy

Each successful transfer creates a unique directory beneath `~/.cache/dbr/paste`, or your `--output-dir`. On Unix, transfer directories have mode `0700` and image files mode `0600`. Incomplete transfers do not leave image files behind.

Output paths must not contain symlink components, `..`, or control characters. Use a physical directory path if your usual path contains a symlink.

**Successful images stay on disk until you remove them.** There is no automatic expiry. Delete the individual image directory when the receiving application no longer needs it. Container storage still follows your container's normal persistence rules.

Enabling clipboard access lets clients with the host token request supported clipboard images. There is no continuous synchronization or per-request host confirmation. Disable access by stopping the daemon and starting it without `--allow-clipboard`.

Transfers use the bridge's existing TCP transport, which is **not encrypted**. Keep its ports within your trusted host/container network; token authentication does not protect against network eavesdropping.

## Protocol and troubleshooting

The CLI opens a dedicated connection to the existing host **data port** (default `19286`). It sends one JSON-line `ClipboardRead` message containing the requested format and authentication token. After checking opt-in and authentication, the host replies with a JSON-line `ClipboardReady` header containing the actual format and byte count, followed by raw image bytes. Failures return `ClipboardError`. Image bytes do not pass through terminal paste or the control channel, and the container daemon need not be running for this command.

- **Sharing disabled:** stop the current host daemon, then start it with `--allow-clipboard`.
- **Authentication failed:** supply the same token the host daemon uses. `--no-auth` cannot grant clipboard access.
- **No requested image:** copy image contents; try `--format auto` if JPEG is unavailable. Confirm the daemon can access your logged-in desktop session.
- **Connection or protocol error after upgrading:** upgrade both binaries and restart the host with clipboard access enabled. Check the host address and data port.
- **tmux target error:** run from container tmux and use an exact pane ID on the server identified by `TMUX`.
- **Transfer already in progress:** retry after the current transfer finishes; the host permits one at a time.

Written by Codex; exact model unavailable.
