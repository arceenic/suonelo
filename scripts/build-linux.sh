#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
if [[ "$(uname -sm)" != 'Linux x86_64' ]]; then
  printf 'Run this build on x86-64 Linux.\n' >&2
  exit 1
fi

target=x86_64-unknown-linux-gnu
export CARGO_TARGET_DIR="$PWD/src-tauri/target"
export NEXT_PUBLIC_SUONELO_PLATFORM=linux NEXT_TELEMETRY_DISABLED=1
export CARGO_ENCODED_RUSTFLAGS
printf -v CARGO_ENCODED_RUSTFLAGS '%s\x1f%s\x1f%s\x1f%s' \
  "--remap-path-prefix=$PWD=/build/source" "--remap-path-prefix=$HOME=/build/home" -C debuginfo=0

mkdir -p -- "$CARGO_TARGET_DIR"
exec 9>"$CARGO_TARGET_DIR/build-linux.lock"
flock 9

version=$(node -p 'require("./src-tauri/tauri.conf.json").version')
npm ci --no-audit
npm run tauri -- build --target "$target" --no-bundle -- --locked
npm run tauri -- bundle --target "$target" --bundles deb,appimage \
  --config '{"bundle":{"useLocalToolsDir":true}}'

bundle="$CARGO_TARGET_DIR/$target/release/bundle"
name="suonelo-$version-linux-x86_64"
stage=$(mktemp -d "$CARGO_TARGET_DIR/.linux-artifacts.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
install -m 0644 "$bundle/deb/suonelo_${version}_amd64.deb" "$stage/$name.deb"
install -m 0755 "$bundle/appimage/suonelo_${version}_amd64.AppImage" "$stage/$name.AppImage"
python3 scripts/check-linux-package.py "$stage/$name.deb"
(
  cd -- "$stage"
  sha256sum "$name.deb" "$name.AppImage" > SHA256SUMS.txt
)

output="$PWD/$name"
build_number=1
while [[ -e "$output" || -L "$output" ]]; do
  ((build_number += 1))
  output="$PWD/$name-$build_number"
done
mv -nT -- "$stage" "$output"
if [[ -d "$stage" ]]; then
  printf 'Build output already exists at %s\n' "$output" >&2
  exit 1
fi
printf 'Linux build artifacts written to %s\n' "$output"
