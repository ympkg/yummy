#!/bin/bash
set -euo pipefail

REPO="ympkg/yummy"
INSTALL_DIR="${YM_INSTALL_DIR:-$HOME/.ym/bin}"

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)   TARGET_OS="unknown-linux-gnu" ;;
  Darwin)  TARGET_OS="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*) TARGET_OS="pc-windows-msvc" ;;
  *)       echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64|amd64) TARGET_ARCH="x86_64" ;;
  aarch64|arm64) TARGET_ARCH="aarch64" ;;
  *)             echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"

# Get latest version from GitHub API
echo "Fetching latest release..."
RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
VERSION=$(curl -fsSL "$RELEASE_URL" | grep '"tag_name"' | sed 's/.*"v\(.*\)".*/\1/')

if [ -z "$VERSION" ]; then
  echo "Failed to fetch latest version. Trying dev release..."
  RELEASE_URL="https://api.github.com/repos/${REPO}/releases"
  VERSION=$(curl -fsSL "$RELEASE_URL" | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')
fi

if [ -z "$VERSION" ]; then
  echo "Error: Could not determine version to install."
  exit 1
fi

echo "Installing ym v${VERSION} for ${TARGET}..."

# Determine archive format
if [ "$TARGET_OS" = "pc-windows-msvc" ]; then
  ARCHIVE="ym-${VERSION}-${TARGET}.zip"
else
  ARCHIVE="ym-${VERSION}-${TARGET}.tar.gz"
fi

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"

# Download and extract
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${DOWNLOAD_URL}..."
curl -fsSL "$DOWNLOAD_URL" -o "${TMPDIR}/${ARCHIVE}"

mkdir -p "$INSTALL_DIR"

if [ "$TARGET_OS" = "pc-windows-msvc" ]; then
  unzip -qo "${TMPDIR}/${ARCHIVE}" -d "$TMPDIR"
  cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym.exe" "$INSTALL_DIR/"
  cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym.exe" "$INSTALL_DIR/ymc.exe"
  if [ -f "${TMPDIR}/ym-${VERSION}-${TARGET}/ym-agent.jar" ]; then
    cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym-agent.jar" "$INSTALL_DIR/"
  fi
else
  tar xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"
  cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym" "$INSTALL_DIR/"
  cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym" "$INSTALL_DIR/ymc"
  chmod +x "$INSTALL_DIR/ym" "$INSTALL_DIR/ymc"
  if [ -f "${TMPDIR}/ym-${VERSION}-${TARGET}/ym-agent.jar" ]; then
    cp "${TMPDIR}/ym-${VERSION}-${TARGET}/ym-agent.jar" "$INSTALL_DIR/"
  fi
fi

echo ""
echo "✓ Installed ym v${VERSION} to ${INSTALL_DIR}"
echo ""

# Check if install dir is in PATH
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "Add this to your shell profile:"
    echo ""
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    ;;
esac

echo "Run 'ym --version' to verify."
