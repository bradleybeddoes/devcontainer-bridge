# Paste host clipboard images into a devcontainer

`dbr paste` saves an image from the host clipboard as a local PNG or JPEG and prints its absolute path. It works with any application that accepts a file path. An optional tmux integration types that path into a chosen pane without pressing Enter.

## Enable on the host

Clipboard support and persistent configuration are available in **v0.4.0**. Install that version on the host and in the container using the instructions below, then create the host configuration directory if needed with `mkdir -p ~/.config/dbr`. In `~/.config/dbr/config.toml` **on the host**, add or update this section without replacing your other settings:

```toml
[clipboard]
enabled = true
```

Apply the setting to the running daemon:

```sh
/usr/local/bin/dbr restart
```

`ensure` starts a daemon only if one is absent; it does not update an already-running daemon's configuration. `restart` stops the daemon, temporarily interrupting forwarded connections, and starts it with the current configuration. New `ensure`, `restart`, and `host-daemon` processes read this setting, so it survives normal restarts.

For one run, `host-daemon --allow-clipboard` enables access and `host-daemon --no-clipboard` disables it regardless of the saved setting. Authentication must remain enabled: an effective clipboard-enabled configuration cannot be combined with `--no-auth`.

The host uses its existing token, or generates one at `~/.config/dbr/auth-token` on first start. The container must have the same token through its existing token configuration or an explicit `--auth-token-file`.

`dbr paste` checks `--auth-token`, `--auth-token-file`, `DCBRIDGE_AUTH_TOKEN`, and `DCBRIDGE_AUTH_TOKEN_FILE` in that order, then `/run/secrets/dbr-auth-token` and `~/.config/dbr/auth-token`. An explicitly configured missing or invalid token is an error; it does not silently fall back.

macOS uses the built-in `osascript` and AppKit clipboard APIs. Run the daemon as your logged-in desktop user. Linux requires `wl-paste` from `wl-clipboard` for Wayland or `xclip` for X11, with the desktop session's environment available to the daemon.

## Install and keep it across restarts

### Mac installation

If an older daemon is running, stop it with its existing `dbr stop` command before installing. This temporarily interrupts forwarded connections.

Install the pinned release:

```sh
curl -fsSL https://github.com/bradleybeddoes/devcontainer-bridge/releases/download/v0.4.0/install.sh | DBR_VERSION=v0.4.0 bash
/usr/local/bin/dbr --version
/usr/local/bin/dbr paste --help
```

The installer verifies the downloaded binary's checksum, installs `/usr/local/bin/dbr`, and creates the `dbr-open` hardlink. It may request sudo to write that directory. An earlier source build at `~/.cargo/bin/dbr` may precede it on `PATH`; check `command -v dbr` and update your PATH or existing startup commands to select `/usr/local/bin/dbr`.

After enabling the TOML setting above, add this to your Mac shell startup file, such as `~/.zshrc`:

```sh
/usr/local/bin/dbr ensure
```

Opening a terminal after logging in or rebooting starts the configured host daemon. It is safe to invoke `ensure` from subsequent terminals. This setup starts the daemon when you begin terminal work. Keep one startup mechanism: if an existing LaunchAgent or another installer already owns your host daemon, update that mechanism to the new executable and remove the duplicate shell startup command.

### Devcontainer rebuilds

Pin the released binary in your existing feature configuration. Merge these fields into `.devcontainer/devcontainer.json`, preserving other features and lifecycle commands:

```jsonc
{
  "features": {
    "ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:0": {
      "version": "v0.4.0"
    }
  },
  "initializeCommand": ["/usr/local/bin/dbr", "ensure"]
}
```

The `:0` suffix selects the devcontainer feature's major version; its `version` option selects the **dbr binary release**, including the `v` prefix. If your configuration already uses this feature, update its existing entry instead of adding a second one. Rebuild the devcontainer to install the pinned binary. The feature creates `dbr-open` and retains its normal container-daemon entrypoint.

`initializeCommand` runs on the host during container creation and subsequent starts, so it also starts the Mac daemon when you start a container after reboot. This example assumes the devcontainer CLI runs on your Mac. Preserve an existing initialization command by adding the dbr invocation to its setup sequence. See the [Dev Container lifecycle specification](https://github.com/devcontainers/spec/blob/main/docs/specs/devcontainerjson-reference.md#lifecycle-scripts).

If you followed the earlier source-build workaround, remove its extra dbr builder stage, `DBR_REF`/`DBR_REPOSITORY` build arguments, and post-create binary replacement before rebuilding. Preserve unrelated project setup. No custom dbr Dockerfile or post-create installer is needed for the release.

Keep your existing host-token mount or environment configuration and the tmux binding in managed dotfiles. After rebuilding, verify `dbr --version` and `dbr paste --help`, then test a real clipboard image. Do not put the token in the image or feature options.

### Updates and rollback

To update, choose the same release for the Mac installer (`DBR_VERSION`) and the feature's `version` option, then rebuild the container. Restart the Mac host daemon after installation so it uses the new executable; replacing the binary alone does not replace an already-running process.

To disable clipboard sharing permanently, set `[clipboard] enabled = false` and restart the host. To roll back, stop the daemon, reinstall the desired earlier release on the Mac, change the feature's version pin, and rebuild. Remove the `[clipboard]` section when returning to a version that predates it. Retained images are not deleted by rollback.

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

Enabling clipboard access lets clients with the host token request supported clipboard images. There is no continuous synchronization or per-request host confirmation. Disable access by setting `[clipboard] enabled = false` and restarting the daemon, or use `host-daemon --no-clipboard` for one run.

Transfers use the bridge's existing TCP transport, which is **not encrypted**. Keep its ports within your trusted host/container network; token authentication does not protect against network eavesdropping.

## Protocol and troubleshooting

The CLI opens a dedicated connection to the existing host **data port** (default `19286`). It sends one JSON-line `ClipboardRead` message containing the requested format and authentication token. After checking opt-in and authentication, the host replies with a JSON-line `ClipboardReady` header containing the actual format and byte count, followed by raw image bytes. Failures return `ClipboardError`. Image bytes do not pass through terminal paste or the control channel, and the container daemon need not be running for this command.

- **Sharing disabled:** set `[clipboard] enabled = true` on the host and restart it. `ensure` does not change an existing daemon.
- **Authentication failed:** supply the same token the host daemon uses. `--no-auth` cannot grant clipboard access.
- **No requested image:** copy image contents; try `--format auto` if JPEG is unavailable. Confirm the daemon can access your logged-in desktop session.
- **Connection or protocol error after upgrading:** upgrade both binaries and restart the host with clipboard access enabled. Check the host address and data port.
- **tmux target error:** run from container tmux and use an exact pane ID on the server identified by `TMUX`.
- **Transfer already in progress:** retry after the current transfer finishes; the host permits one at a time.

Written by Codex; exact model unavailable.
