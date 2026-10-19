#!/usr/bin/env bash
set -euo pipefail

repo=${CSLIM_REPO:-aak1247/cslim}
tag=latest
archive_path=
with_timer=0
while (($#)); do
  case "$1" in
    --version) tag="${2:?--version requires a tag}"; shift 2 ;;
    --from) archive_path="${2:?--from requires an archive path}"; shift 2 ;;
    --timer) with_timer=1; shift ;;
    -h|--help)
      echo 'Usage: bash install.sh [--version v0.5.0] [--timer] [--from /path/to/cslim-x86_64-unknown-linux-musl.tar.gz]'
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo 'Prebuilt cslim binaries currently support Linux x86_64 only. Build from source on other platforms.' >&2
  exit 1
fi
asset=cslim-x86_64-unknown-linux-musl.tar.gz
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
if [[ -n "$archive_path" ]]; then
  cp -- "$archive_path" "$scratch/$asset"
  cp -- "$archive_path.sha256" "$scratch/$asset.sha256"
else
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    if [[ "$tag" == latest ]]; then
      gh release download --repo "$repo" --pattern "$asset" --pattern "$asset.sha256" --dir "$scratch"
    else
      gh release download "$tag" --repo "$repo" --pattern "$asset" --pattern "$asset.sha256" --dir "$scratch"
    fi
  else
    if [[ "$tag" == latest ]]; then
      base="https://github.com/$repo/releases/latest/download"
    else
      base="https://github.com/$repo/releases/download/$tag"
    fi
    curl --proto '=https' --tlsv1.2 -fsSL "$base/$asset" -o "$scratch/$asset"
    curl --proto '=https' --tlsv1.2 -fsSL "$base/$asset.sha256" -o "$scratch/$asset.sha256"
  fi
fi
(cd "$scratch" && sha256sum -c "$asset.sha256")
tar -xzf "$scratch/$asset" -C "$scratch"
package="$scratch/cslim"
"$package/cargo-cslim" --version
bin_dir="${HOME:?}/.local/bin"
libexec_dir="$HOME/.local/libexec/cslim"
if [[ -e "$bin_dir/cslim" && ! -L "$bin_dir/cslim" ]]; then
  echo "Existing non-symlink $bin_dir/cslim; refusing to overwrite it." >&2
  exit 1
fi
if ! command -v sccache >/dev/null 2>&1 && [[ ! -x "$bin_dir/sccache" ]]; then
  bash "$package/scripts/install-sccache.sh"
fi
mkdir -p "$bin_dir" "$libexec_dir"
install -m 755 "$package/cargo-cslim" "$bin_dir/cargo-cslim.new"
mv -f "$bin_dir/cargo-cslim.new" "$bin_dir/cargo-cslim"
ln -sfn cargo-cslim "$bin_dir/cslim"
install -m 755 "$package/adapters/cslim-dev" "$bin_dir/cslim-dev"
install -m 755 "$package/adapters/cargo-shim" "$libexec_dir/cargo"
if ((with_timer)); then
  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir"
  install -m 644 "$package/deploy/cslim-gc.service" "$package/deploy/cslim-gc.timer" "$unit_dir/"
  systemctl --user daemon-reload
  systemctl --user enable --now cslim-gc.timer
fi
echo "Installed cslim to $bin_dir. Add it to PATH, then run: cslim status"
