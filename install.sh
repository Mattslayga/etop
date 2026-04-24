#!/bin/sh
set -eu

OWNER="Mattslayga"
REPO="etop"
INSTALL_DIR="${HOME}/.local/bin"
VERSION=""

usage() {
  cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--dir PATH]

Installs the Apple Silicon macOS etop release binary from GitHub Releases into
~/.local/bin by default.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --dir)
      INSTALL_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "install.sh: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "install.sh: etop currently supports macOS only." >&2
  exit 1
fi

if [ "$(uname -m)" != "arm64" ]; then
  echo "install.sh: published etop binaries currently target Apple Silicon only." >&2
  exit 1
fi

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n 1)"
fi

if [ -z "$VERSION" ]; then
  echo "install.sh: unable to determine the latest etop release." >&2
  exit 1
fi

ASSET="etop-${VERSION}-macos-arm64.tar.gz"
URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${ASSET}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

echo "Installing etop ${VERSION} into ${INSTALL_DIR}"
mkdir -p "$INSTALL_DIR"
curl -fsSL "$URL" -o "${tmpdir}/${ASSET}"
tar -xzf "${tmpdir}/${ASSET}" -C "$tmpdir"
install -m 0755 "${tmpdir}/etop" "${INSTALL_DIR}/etop"

echo "Installed ${INSTALL_DIR}/etop"
echo "Run: ${INSTALL_DIR}/etop --version"
