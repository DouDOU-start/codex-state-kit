#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_dir="$project_root/src-tauri/resources/mihomo"
download_dir="$project_root/target/mihomo-download"
version="v1.19.31"
asset="mihomo-linux-amd64-${version}.gz"
expected="d5e74bbddbdfff49a1aef7775bf5911da59f0d7196ed509a0ac914b3653dd5f1"
archive="$download_dir/$asset"
url="https://github.com/MetaCubeX/mihomo/releases/download/${version}/${asset}"

mkdir -p "$download_dir" "$resource_dir"
if [[ ! -f "$archive" ]] || [[ "$(sha256sum "$archive" | awk '{print tolower($1)}')" != "$expected" ]]; then
  curl -fL --retry 3 --connect-timeout 15 --max-time 180 "$url" -o "$archive"
fi
actual="$(sha256sum "$archive" | awk '{print tolower($1)}')"
[[ "$actual" == "$expected" ]] || { echo "mihomo Linux archive checksum mismatch" >&2; exit 1; }

tmp="$download_dir/mihomo-linux-amd64"
gzip -dc "$archive" > "$tmp"
binary_info="$(file -b "$tmp")"
[[ "$binary_info" == *"x86-64"* || "$binary_info" == *"x86_64"* ]] || {
  echo "Mihomo architecture mismatch: expected x86_64, got $binary_info" >&2
  exit 1
}

rm -f "$resource_dir/mihomo" "$resource_dir/mihomo.exe"
install -m 0755 "$tmp" "$resource_dir/mihomo"
echo "Bundled mihomo ${version} (Linux amd64); SHA-256 verified."
