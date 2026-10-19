#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
cargo install --force --path . --locked --root "$HOME/.local"
if [[ -e "$HOME/.local/bin/cslim" && ! -L "$HOME/.local/bin/cslim" ]]; then
  echo '已有同名 cslim 可执行文件，未覆盖' >&2
  exit 1
fi
ln -sfn cargo-cslim "$HOME/.local/bin/cslim"
install -m 755 adapters/cslim-dev "$HOME/.local/bin/"
mkdir -p "$HOME/.local/libexec/cslim"
install -m 755 adapters/cargo-shim "$HOME/.local/libexec/cslim/cargo"
if [[ "${1:-}" == "--timer" ]]; then
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir"
  install -m 644 deploy/cslim-gc.service deploy/cslim-gc.timer "$unit_dir/"
  systemctl --user daemon-reload
  systemctl --user enable --now cslim-gc.timer
fi
