#!/usr/bin/env bash
# Install the latest wtm release into ~/.local/bin.
# Usage: curl -fsSL https://raw.githubusercontent.com/faulker/wtm/main/install.sh | bash
set -euo pipefail

REPO="faulker/wtm"
DEST_DIR="${WTM_INSTALL_DIR:-$HOME/.local/bin}"
DEST="$DEST_DIR/wtm"

die() {
  echo "install.sh: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' is required but was not found on PATH"
}

need curl
need tar

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) die "unsupported OS: $(uname -s); install a release manually from https://github.com/${REPO}/releases" ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

case "${os}-${arch}" in
  macos-aarch64) triple=aarch64-apple-darwin ;;
  macos-x86_64) triple=x86_64-apple-darwin ;;
  linux-x86_64) triple=x86_64-unknown-linux-gnu ;;
  linux-aarch64) triple=aarch64-unknown-linux-gnu ;;
  *) die "no release is built for ${os}-${arch}" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  sha_cmd=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  sha_cmd=(shasum -a 256)
else
  die "need sha256sum or shasum to verify the download"
fi

echo "Looking up the latest wtm release…"
resolved="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
tag="${resolved##*/releases/tag/}"
tag="${tag%%[?#]*}"
tag="${tag%/}"
[[ -n "$tag" && "$tag" != "latest" ]] || die "could not resolve the latest release tag from ${resolved}"

asset="wtm-${tag}-${triple}.tar.gz"
base="https://github.com/${REPO}/releases/download/${tag}"
work="$(mktemp -d "${TMPDIR:-/tmp}/wtm-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT

echo "Downloading ${asset}…"
curl -fsSL "${base}/${asset}" -o "${work}/${asset}"
curl -fsSL "${base}/checksums-sha256.txt" -o "${work}/checksums-sha256.txt"

expected="$("${sha_cmd[@]}" "${work}/${asset}" | awk '{print $1}')"
listed="$(awk -v name="$asset" '
  {
    hash = $1
    file = $2
    sub(/^\*/, "", file)
    n = split(file, parts, "/")
    base = parts[n]
    if (base == name) { print hash; exit }
  }
' "${work}/checksums-sha256.txt")"
[[ -n "$listed" ]] || die "checksums-sha256.txt does not list ${asset}"
[[ "$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')" == "$(printf '%s' "$listed" | tr '[:upper:]' '[:lower:]')" ]] \
  || die "checksum mismatch for ${asset}: expected ${listed}, got ${expected}"

tar -xzf "${work}/${asset}" -C "$work"
[[ -f "${work}/wtm" ]] || die "${asset} did not contain a wtm binary"

mkdir -p "$DEST_DIR"
# Stage next to the destination so the final move is a same-filesystem rename.
staged="${DEST_DIR}/.wtm-install.$$"
cp "${work}/wtm" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$DEST"

version="$("$DEST" --version 2>/dev/null | head -n1 || true)"
echo "Installed ${version:-wtm} to ${DEST}"
case ":$PATH:" in
  *":${DEST_DIR}:"*) ;;
  *)
    echo "Note: add ${DEST_DIR} to your PATH if 'wtm' is not found."
    ;;
esac
