# Paste host clipboard images into a devcontainer

`dbr paste` saves an image from the host clipboard as a local PNG or JPEG and prints its absolute path. It works with any application that accepts a file path. An optional tmux integration types that path into a chosen pane without pressing Enter.

## Enable on the host

Both binaries must include clipboard support. This feature is not yet in a published release: the standard installer and a devcontainer feature using `version: "latest"` still install the released binary. Use the source installation below until a supporting release is available.

Install the source version described below first. Then create the host configuration directory if needed with `mkdir -p ~/.config/dbr`. In `~/.config/dbr/config.toml` **on the host**, add or update this section without replacing your other settings:

```toml
[clipboard]
enabled = true
```

Apply the setting to the running daemon:

```sh
"$HOME/.cargo/bin/dbr" restart
```

`ensure` starts a daemon only if one is absent; it does not update an already-running daemon's configuration. `restart` stops the daemon, temporarily interrupting forwarded connections, and starts it with the current configuration. New `ensure`, `restart`, and `host-daemon` processes read this setting, so it survives normal restarts.

For one run, `host-daemon --allow-clipboard` enables access and `host-daemon --no-clipboard` disables it regardless of the saved setting. Authentication must remain enabled: an effective clipboard-enabled configuration cannot be combined with `--no-auth`.

The host uses its existing token, or generates one at `~/.config/dbr/auth-token` on first start. The container must have the same token through its existing token configuration or an explicit `--auth-token-file`.

`dbr paste` checks `--auth-token`, `--auth-token-file`, `DCBRIDGE_AUTH_TOKEN`, and `DCBRIDGE_AUTH_TOKEN_FILE` in that order, then `/run/secrets/dbr-auth-token` and `~/.config/dbr/auth-token`. An explicitly configured missing or invalid token is an error; it does not silently fall back.

macOS uses the built-in `osascript` and AppKit clipboard APIs. Run the daemon as your logged-in desktop user. Linux requires `wl-paste` from `wl-clipboard` for Wayland or `xclip` for X11, with the desktop session's environment available to the daemon.

## Install from source and keep it across restarts

### Mac installation

With Git and Rust installed on the Mac, clone the source branch into a new directory, then select the same reviewed commit used by the container build:

```sh
git clone --branch feat/clipboard-paste https://github.com/bradleybeddoes/devcontainer-bridge.git dbr-clipboard
cd dbr-clipboard
git checkout --detach 4ff17185fb8f85e726bd0114bb2abc23b3c77524
git rev-parse HEAD
cargo install --locked --path . --force
"$HOME/.cargo/bin/dbr" paste --help
"$HOME/.cargo/bin/dbr" host-daemon --help
```

This installs to `~/.cargo/bin/dbr`. An older `/usr/local/bin/dbr` may precede it on `PATH`; the commands here deliberately use the new executable's absolute path. To use the new version by name, place `export PATH="$HOME/.cargo/bin:$PATH"` in your Mac shell configuration and verify `command -v dbr`.

After enabling the TOML setting above, add this to your Mac shell startup file, such as `~/.zshrc`:

```sh
"$HOME/.cargo/bin/dbr" ensure
```

Opening a terminal after logging in or rebooting starts the configured host daemon. It is safe to invoke `ensure` from subsequent terminals. This setup starts the daemon when you begin terminal work, rather than before login. Keep one startup mechanism: if an existing LaunchAgent or another installer already owns your host daemon, update that mechanism to the new executable and remove the duplicate shell startup command.

### Devcontainer rebuilds

Installing a binary interactively inside a container does not survive a rebuild. Build the chosen source commit into the image, then install it **after** devcontainer features. The existing dbr feature can remain: it supplies its normal entrypoint, while the post-create step replaces its older released binary.

Add a builder stage to your existing `.devcontainer/Dockerfile`. Keep your own final base image; the Debian example below is illustrative. The Alpine builder produces a static musl executable for the target container architecture.

```dockerfile
FROM rust:1.93-alpine AS dbr-build
RUN apk add --no-cache git musl-dev
ARG DBR_REPOSITORY
ARG DBR_REF
WORKDIR /src/dbr
RUN git init . \
    && git remote add origin "$DBR_REPOSITORY" \
    && git fetch --depth 1 origin "$DBR_REF" \
    && git checkout --detach FETCH_HEAD \
    && test "$(git rev-parse HEAD)" = "$DBR_REF" \
    && cargo build --release --locked

FROM mcr.microsoft.com/devcontainers/base:bookworm
COPY --from=dbr-build /src/dbr/target/release/dbr /opt/dbr-clipboard/dbr
```

Merge these fields into `.devcontainer/devcontainer.json`, preserving your existing features and lifecycle commands. `DBR_REF` pins the same source commit as the Mac installation. `initializeCommand` runs on the Mac and starts the host daemon if needed when the container starts, including after reboot:

```jsonc
{
  "build": {
    "dockerfile": "Dockerfile",
    "args": {
      "DBR_REPOSITORY": "https://github.com/bradleybeddoes/devcontainer-bridge.git",
      "DBR_REF": "4ff17185fb8f85e726bd0114bb2abc23b3c77524"
    }
  },
  "initializeCommand": ["${localEnv:HOME}/.cargo/bin/dbr", "ensure"],
  "postCreateCommand": "sh .devcontainer/install-dbr.sh"
}
```

The initialization command assumes the devcontainer CLI runs on the Mac where dbr is installed. If your existing build context or Dockerfile location differs, keep those paths consistent with your project. Preserve an existing `initializeCommand` by adding the dbr invocation to its setup sequence.

Create `.devcontainer/install-dbr.sh` in the project:

```sh
#!/bin/sh
set -eu

# The normal devcontainer user must have passwordless sudo.
sudo install -m 0755 /opt/dbr-clipboard/dbr /usr/local/bin/.dbr-clipboard-new
sudo mv -f /usr/local/bin/.dbr-clipboard-new /usr/local/bin/dbr
sudo ln -f /usr/local/bin/dbr /usr/local/bin/dbr-open
/usr/local/bin/dbr paste --help >/dev/null
```

For a container running as root, omit `sudo`. If `postCreateCommand` already exists, append this script to its existing command with `&&`; do not replace the project's setup. Keep these installation steps in one ordered command, because object-form lifecycle commands run in parallel. See the [Dev Container lifecycle specification](https://github.com/devcontainers/spec/blob/main/docs/specs/devcontainerjson-reference.md#lifecycle-scripts).

Replacing the executable by renaming a new file also works while the previous daemon is running. That already-running process keeps its old code until the container next starts; `dbr paste` uses the new CLI and connects directly to the host, so it does not depend on that process. Future feature entrypoint launches use the replaced binary.

Keep the Dockerfile, source commit pin, installation script, and tmux binding in your project configuration or managed dotfiles. Rebuild the devcontainer and verify `dbr paste --help`, then test a real clipboard image. Keep the existing host-token mount or environment configuration; no secret belongs in the Dockerfile or build arguments.

### Updates and rollback

To update, build a reviewed source commit on the Mac, update `DBR_REF` to the same published commit, and rebuild the devcontainer. Restart the Mac host daemon so it uses the installed executable. Reinstalling the binary alone does not replace an already-running process.

To disable clipboard sharing permanently, set `[clipboard] enabled = false` and restart the host. To return to the released binaries, remove the custom container builder/post-create installation and rebuild using the original feature; reinstall the desired Mac release and update your shell startup and initialization commands to its executable. Retained images are not deleted by rollback.

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
