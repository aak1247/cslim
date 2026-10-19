#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo 'This bundled sccache installer supports Linux x86_64 only.' >&2
  exit 1
fi
version=0.18.0
archive="sccache-v${version}-x86_64-unknown-linux-musl.tar.gz"
expected=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89
install_dir="${HOME:?}/.local/bin"
if command -v sccache >/dev/null 2>&1 || [[ -x "$install_dir/sccache" ]]; then
  echo 'sccache is already installed; keeping it.'
  exit 0
fi
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL "https://github.com/mozilla/sccache/releases/download/v${version}/${archive}" -o "$scratch/$archive"
printf '%s  %s\n' "$expected" "$scratch/$archive" | sha256sum -c -
member=$(tar -tzf "$scratch/$archive" | awk '/\/sccache$/ { print; exit }')
if [[ -z "$member" ]]; then
  echo 'sccache executable is missing from the verified archive.' >&2
  exit 1
fi
tar -xzf "$scratch/$archive" -C "$scratch" "$member"
mkdir -p "$install_dir"
install -m 755 "$scratch/$member" "$install_dir/sccache"
echo "Installed sccache ${version} to $install_dir/sccache"
