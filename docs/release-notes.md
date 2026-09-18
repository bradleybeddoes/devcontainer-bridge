# v0.5.0

## Multiple clipboard images

- `dbr paste` now transfers every image in a macOS Finder selection instead of refusing multi-file copies.
- Images are saved side by side as `clipboard-1`, `clipboard-2`, and so on inside one transfer directory. Every path is printed, one per line, and `--tmux-target` inserts all of them shell-quoted and space-separated.
- A single image is unchanged: it is still saved as `clipboard.png` or `clipboard.jpg` with no number suffix.
- A selection is limited to 16 images and 64 MiB in total, with the existing 20 MiB cap per image.
- A selection is transferred whole or not at all. Any file that is not a PNG or JPEG matching the requested format fails the request rather than silently saving the rest.

## Upgrade

Multi-image transfer requires **both** sides on v0.5.0. Install the v0.5.0 host binary and pin the devcontainer feature to `v0.5.0`, then rebuild the container.

Single-image transfer stays wire-compatible in both directions. A v0.5.0 host omits the new `ClipboardBatchReady` header when there is exactly one image, so v0.4.x container clients continue to work unchanged. A v0.4.x client asked to receive a multi-image selection reports a protocol error; copy one file at a time until it is upgraded.

Written by Claude (claude-opus-5).
