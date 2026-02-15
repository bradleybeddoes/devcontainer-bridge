#!/usr/bin/env bash
set -euo pipefail

REPO="bradleybeddoes/devcontainer-bridge"
INSTALL_DIR="/usr/local/bin"

# Determine version (latest release if not specified)
VERSION="${DBR_VERSION:-}"
if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')
  if [ -z "$VERSION" ]; then
    echo "Error: could not determine latest release version" >&2
    exit 1
  fi
fi

# Detect OS
OS=$(uname -s)
case "$OS" in
  Linux)  os_target="unknown-linux-musl" ;;
  Darwin) os_target="apple-darwin" ;;
  *)
    echo "Error: unsupported OS: $OS" >&2
    exit 1
    ;;
esac

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)  arch_target="x86_64" ;;
  aarch64|arm64)  arch_target="aarch64" ;;
  *)
    echo "Error: unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${arch_target}-${os_target}"
BINARY_NAME="dbr-${TARGET}"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY_NAME}"
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"

echo "Installing dbr ${VERSION} for ${TARGET}..."

# Download binary and checksum
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL -o "${TMPDIR}/dbr" "$DOWNLOAD_URL"
curl -fsSL -o "${TMPDIR}/dbr.sha256" "$CHECKSUM_URL"

# Verify checksum
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

# Install
chmod +x dbr
echo "Installing to ${INSTALL_DIR}/dbr..."
if [ -w "$INSTALL_DIR" ]; then
  cp dbr "${INSTALL_DIR}/dbr"
else
  sudo cp dbr "${INSTALL_DIR}/dbr"
fi

# Create dbr-open hardlink
echo "Creating dbr-open hardlink..."
if [ -w "$INSTALL_DIR" ]; then
  ln -f "${INSTALL_DIR}/dbr" "${INSTALL_DIR}/dbr-open"
else
  sudo ln -f "${INSTALL_DIR}/dbr" "${INSTALL_DIR}/dbr-open"
fi

echo "Done! dbr ${VERSION} installed to ${INSTALL_DIR}/dbr"
echo "dbr-open hardlink created at ${INSTALL_DIR}/dbr-open"
