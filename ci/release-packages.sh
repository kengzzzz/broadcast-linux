#!/usr/bin/env bash
# Repackages DIST/broadcast-linux-<version>-x86_64.tar.gz as a .deb and an .rpm in DIST.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
dist=$(realpath -- "$1")
nfpm=goreleaser/nfpm:v2.47.0
tarball=$(ls "$dist"/broadcast-linux-*-x86_64.tar.gz)
name=$(basename "$tarball" .tar.gz)
version=${name#broadcast-linux-}
version=${version%-x86_64}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
tar -C "$stage" -xzf "$tarball"
cp -r "$root/packaging" "$stage/$name/"

for packager in deb rpm; do
    docker run --rm --user "$(id -u):$(id -g)" \
        -v "$stage/$name:/src:ro" -v "$dist:/dist" -w /src -e VERSION="$version" \
        "$nfpm" package --config packaging/nfpm.yaml --packager "$packager" --target /dist/
done
ls -l "$dist"
