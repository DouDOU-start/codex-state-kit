#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_dir="$project_root/src-tauri/resources/mihomo"
download_dir="$project_root/target/mihomo-download"
arch="${1:-$(uname -m)}"
version="v1.19.31"

case "$arch" in
  arm64|aarch64)
    asset_arch="arm64"
    expected="d131f44b3deb2a8356f7ac75048ad67a10d53243323951c4f3cda7b672922963"
    ;;
  amd64|x86_64)
    asset_arch="amd64"
    expected="3546681ebef3415e5dcbe7210a61aa80748136e95e6552768fd883df345508ed"
    ;;
  *) echo "Unsupported macOS architecture: $arch" >&2; exit 2 ;;
esac

asset="mihomo-darwin-${asset_arch}-${version}.gz"
url="https://github.com/MetaCubeX/mihomo/releases/download/${version}/${asset}"
archive="$download_dir/$asset"
mkdir -p "$download_dir" "$resource_dir"

if [[ ! -f "$archive" ]] || [[ "$(shasum -a 256 "$archive" | awk '{print tolower($1)}')" != "$expected" ]]; then
  curl -fL --retry 3 --connect-timeout 15 --max-time 180 "$url" -o "$archive"
fi
actual="$(shasum -a 256 "$archive" | awk '{print tolower($1)}')"
[[ "$actual" == "$expected" ]] || { echo "mihomo archive checksum mismatch" >&2; exit 1; }

tmp="$download_dir/mihomo-darwin-${asset_arch}"
gzip -dc "$archive" > "$tmp"
rm -f "$resource_dir/mihomo" "$resource_dir/mihomo.exe"
install -m 0755 "$tmp" "$resource_dir/mihomo"
echo "Bundled mihomo ${version} (Darwin ${asset_arch}); SHA-256 verified."
