#!/usr/bin/env bash
# Builds the release binary and packages it with LICENSE and README.md as
# dist/svalbard-<version>-<target>.tar.gz, for the target of the build host. dist/SHA256SUMS holds
# the SHA-256 of the archive. The release workflow runs it on ubuntu-24.04, so the release target
# is x86_64-unknown-linux-gnu. It needs GNU tar.
#
# Usage: scripts/package.sh
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --locked --release
version=$(target/release/svalbard --version)
target=$(rustc -vV | sed -n 's/^host: //p')
name="svalbard-${version#svalbard }-$target"

rm -rf dist
mkdir -p "dist/$name"
cp target/release/svalbard LICENSE README.md "dist/$name/"
tar --create --gzip --file "dist/$name.tar.gz" --directory dist --owner=0 --group=0 "$name"
rm -r "dist/${name:?}"
(cd dist && sha256sum "$name.tar.gz" >SHA256SUMS)
cat dist/SHA256SUMS
