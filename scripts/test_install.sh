#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
asset="$PWD/dist/cslim-x86_64-unknown-linux-musl.tar.gz"
if [[ ! -f "$asset" ]]; then
  echo "Missing $asset; package the release first." >&2
  exit 1
fi
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/home" "$scratch/fake-bin"
printf '#!/bin/sh\necho sccache-fixture\n' > "$scratch/fake-bin/sccache"
chmod +x "$scratch/fake-bin/sccache"
HOME="$scratch/home" PATH="$scratch/fake-bin:$PATH" bash install.sh --from "$asset"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[[ "$("$scratch/home/.local/bin/cargo-cslim" --version)" == "cslim $version" ]]
[[ -L "$scratch/home/.local/bin/cslim" ]]
[[ -x "$scratch/home/.local/bin/cslim-dev" ]]
[[ -x "$scratch/home/.local/libexec/cslim/cargo" ]]
echo 'PASS: verified release archive installs without Python or a Rust build'
