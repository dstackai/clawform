#!/bin/sh
set -eu

REPO="${CLAWFORM_REPO:-dstackai/clawform}"
INSTALL_DIR="${CLAWFORM_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${CLAWFORM_VERSION:-latest}"

usage() {
  cat <<'EOF'
Clawform installer

Usage:
  sh install.sh [--version <tag>] [--install-dir <path>] [--repo <owner/name>]

Environment overrides:
  CLAWFORM_VERSION      Tag to install (example: v0.1.0, v0.2.0-rc.1)
  CLAWFORM_INSTALL_DIR  Destination directory (default: ~/.local/bin)
  CLAWFORM_REPO         GitHub repo (default: dstackai/clawform)

Examples:
  curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
  CLAWFORM_VERSION=v0.2.0-rc.1 curl -fsSL https://raw.githubusercontent.com/dstackai/clawform/main/install.sh | sh
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || { echo "error: --version requires a value" >&2; exit 1; }
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || { echo "error: --install-dir requires a value" >&2; exit 1; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --install-dir=*)
      INSTALL_DIR="${1#*=}"
      shift
      ;;
    --repo)
      [ "$#" -ge 2 ] || { echo "error: --repo requires a value" >&2; exit 1; }
      REPO="$2"
      shift 2
      ;;
    --repo=*)
      REPO="${1#*=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

detect_os() {
  case "$(uname -s)" in
    Linux) echo "linux" ;;
    Darwin) echo "darwin" ;;
    *)
      echo "error: unsupported OS: $(uname -s)" >&2
      exit 1
      ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64" ;;
    arm64|aarch64) echo "aarch64" ;;
    *)
      echo "error: unsupported architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

sha256_file() {
  file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return
  fi
  if command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | awk '{print $NF}'
    return
  fi
  echo "error: no SHA-256 tool found (need shasum, sha256sum, or openssl)" >&2
  exit 1
}

fetch_latest_stable_tag() {
  api_url="https://api.github.com/repos/${REPO}/releases/latest"
  tag="$(curl -fsSL "$api_url" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  if [ -z "$tag" ]; then
    echo "error: could not resolve latest stable release tag from $api_url" >&2
    exit 1
  fi
  echo "$tag"
}

normalize_tag() {
  raw="$1"
  case "$raw" in
    v*) echo "$raw" ;;
    *) echo "v$raw" ;;
  esac
}

os="$(detect_os)"
arch="$(detect_arch)"

if [ "$VERSION" = "latest" ]; then
  tag="$(fetch_latest_stable_tag)"
else
  tag="$(normalize_tag "$VERSION")"
fi

asset="clawform_${os}_${arch}.tar.gz"
base_url="https://github.com/${REPO}/releases/download/${tag}"
asset_url="${base_url}/${asset}"
checksums_url="${base_url}/SHA256SUMS"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

echo "Installing Clawform ${tag} from ${REPO}"
echo "Detected target: ${os}/${arch}"
echo "Downloading: ${asset}"

curl -fL "$asset_url" -o "${tmp_dir}/${asset}"
curl -fL "$checksums_url" -o "${tmp_dir}/SHA256SUMS"

expected="$(awk -v n="$asset" '$2 == n {print $1}' "${tmp_dir}/SHA256SUMS" | head -n 1)"
if [ -z "$expected" ]; then
  echo "error: checksum for ${asset} not found in SHA256SUMS" >&2
  exit 1
fi

actual="$(sha256_file "${tmp_dir}/${asset}")"
if [ "$expected" != "$actual" ]; then
  echo "error: checksum mismatch for ${asset}" >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
tar -xzf "${tmp_dir}/${asset}" -C "$tmp_dir"

install -m 0755 "${tmp_dir}/clawform" "${INSTALL_DIR}/clawform"
install -m 0755 "${tmp_dir}/cf" "${INSTALL_DIR}/cf"

echo "Installed:"
echo "  ${INSTALL_DIR}/clawform"
echo "  ${INSTALL_DIR}/cf"
echo
echo "If needed, add to PATH:"
echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
