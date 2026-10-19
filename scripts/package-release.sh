#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
target="${1:-x86_64-unknown-linux-musl}"
binary="target/$target/release/cargo-cslim"
if [[ ! -x "$binary" ]]; then
  echo "Missing $binary; run cargo build --release --locked --target $target" >&2
  exit 1
fi
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/cslim/adapters" "$scratch/cslim/deploy" "$scratch/cslim/scripts" dist
install -m 755 "$binary" "$scratch/cslim/cargo-cslim"
install -m 755 install.sh "$scratch/cslim/install.sh"
install -m 755 adapters/cslim-dev adapters/cargo-shim "$scratch/cslim/adapters/"
install -m 755 scripts/install-sccache.sh "$scratch/cslim/scripts/"
install -m 644 deploy/cslim-gc.service deploy/cslim-gc.timer "$scratch/cslim/deploy/"
install -m 644 config.example.toml README.md README.zh-CN.md LICENSE "$scratch/cslim/"
asset="cslim-$target.tar.gz"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner -C "$scratch" -czf "dist/$asset" cslim
(cd dist && sha256sum "$asset" > "$asset.sha256")
echo "Packed dist/$asset and checksum"
