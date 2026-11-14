#!/bin/sh
# Build the macOS release of ATV into dist/: ATV.app inside a .dmg, plus the
# atv command-line tool. Not yet tried on a Mac; report problems as issues.
#
# Needs: Xcode command-line tools, Rust, the Tauri CLI
#   (cargo install tauri-cli --version "^2" --locked).
# Run from the repository:  sh scripts/build-macos.sh
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
echo "Building ATV $version"

(cd crates/atv-gui && cargo tauri build)
cargo build --release -p atv-cli

mkdir -p dist
cp target/release/bundle/dmg/*.dmg dist/
cp target/release/atv "dist/atv-$version-macos"
ls -lh dist
