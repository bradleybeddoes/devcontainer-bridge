#!/usr/bin/env bash
set -euo pipefail

REPO="bradleybeddoes/devcontainer-bridge"
INSTALL_DIR="/usr/local/bin"

# Feature option: version (default "latest")
VERSION="${VERSION:-latest}"

# ---------------------------------------------------------------------------
# Utility: download a URL to a local file using curl or wget
# ---------------------------------------------------------------------------
download() {
  local url="$1"
  local dest="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    echo "Error: neither curl nor wget is available" >&2
    exit 1
  fi
}

# ---------------------------------------------------------------------------
# Resolve "latest" to an actual tag
# ---------------------------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
  echo "Resolving latest release version..."
  TMPVER=$(mktemp)
  download "https://api.github.com/repos/${REPO}/releases/latest" "$TMPVER"
  VERSION=$(grep '"tag_name"' "$TMPVER" | sed -E 's/.*"([^"]+)".*/\1/')
  rm -f "$TMPVER"

  if [ -z "$VERSION" ]; then
    echo "Error: could not determine latest release version" >&2
    exit 1
  fi
fi

echo "Installing dbr ${VERSION}..."

# ---------------------------------------------------------------------------
# Detect architecture
# ---------------------------------------------------------------------------
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)   arch_target="x86_64" ;;
  aarch64|arm64)   arch_target="aarch64" ;;
  *)
    echo "Error: unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

BINARY_NAME="dbr-${arch_target}-unknown-linux-musl"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY_NAME}"
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"

echo "Downloading ${BINARY_NAME}..."

# ---------------------------------------------------------------------------
# Download binary and checksum
# ---------------------------------------------------------------------------
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

download "$DOWNLOAD_URL" "${TMPDIR}/dbr"
download "$CHECKSUM_URL" "${TMPDIR}/dbr.sha256"

# ---------------------------------------------------------------------------
# Verify checksum
# ---------------------------------------------------------------------------
echo "Verifying checksum..."
cd "$TMPDIR"
if command -v sha256sum >/dev/null 2>&1; then
  # Rewrite checksum file to reference local filename
  awk '{print $1 "  dbr"}' dbr.sha256 > dbr.sha256.check
  sha256sum -c dbr.sha256.check
elif command -v shasum >/dev/null 2>&1; then
  EXPECTED=$(awk '{print $1}' dbr.sha256)
  ACTUAL=$(shasum -a 256 dbr | awk '{print $1}')
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Error: checksum mismatch" >&2
    exit 1
  fi
  echo "dbr: OK"
else
  echo "Warning: no sha256sum or shasum found, skipping checksum verification" >&2
fi

# ---------------------------------------------------------------------------
# Install binary and create hardlink
# ---------------------------------------------------------------------------
chmod +x dbr

echo "Installing to ${INSTALL_DIR}/dbr..."
cp dbr "${INSTALL_DIR}/dbr"

echo "Creating dbr-open hardlink..."
ln -f "${INSTALL_DIR}/dbr" "${INSTALL_DIR}/dbr-open"

echo "Installing entrypoint script..."
mkdir -p /usr/local/share/dbr
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cp "${SCRIPT_DIR}/entrypoint.sh" /usr/local/share/dbr/entrypoint.sh
chmod 0755 /usr/local/share/dbr/entrypoint.sh

echo "Done! dbr ${VERSION} installed to ${INSTALL_DIR}/dbr"
echo "dbr-open hardlink created at ${INSTALL_DIR}/dbr-open"
echo "Entrypoint script installed at /usr/local/share/dbr/entrypoint.sh"
