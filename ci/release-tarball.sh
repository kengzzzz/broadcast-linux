#!/usr/bin/env bash
# Builds everything and packs broadcast-linux-<version>-x86_64.tar.gz into OUT.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=$1
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
name="broadcast-linux-$version-x86_64"
stage=$(mktemp -d)/$name
trap 'rm -rf "$(dirname "$stage")"' EXIT

"$root/build.sh" "$stage"
install -Dm644 -t "$stage/share" "$root/packaging/broadcast-linux.service" \
    "$root/packaging/config.toml" "$root/packaging/modules-load.conf" "$root/packaging/modprobe.conf" \
    "$root/packaging/broadcast-linux.desktop" "$root/packaging/broadcast-linux.svg"
install -Dm755 "$root/packaging/install.sh" "$stage/install.sh"
install -Dm644 -t "$stage" "$root/README.md" "$root/CHANGELOG.md" "$root/LICENSE"
install -Dm644 -t "$stage/docs" "$root"/docs/*.md
install -Dm644 "$root/wine-nvcuda/LICENSE.md" "$stage/LICENSE.nvcuda.md"
install -Dm644 "$root/vfx-api/LICENSE" "$stage/LICENSE.nvidia-vfx-headers"
install -Dm644 "$root/packaging/LICENSE.libjpeg-turbo" "$stage/LICENSE.libjpeg-turbo"
install -Dm644 "$root/packaging/LICENSE.fonts" "$stage/LICENSE.fonts"
{
    echo "broadcast-linux $version"
    echo "built on: $(. /etc/os-release && echo "$PRETTY_NAME"), $(ldd --version | head -1)"
    echo "wine: $(wine --version 2>/dev/null || winegcc --version | head -1)"
    echo "rustc: $(rustc --version)"
} > "$stage/BUILD-INFO"
mkdir -p "$out"
tar -C "$(dirname "$stage")" -czf "$out/$name.tar.gz" "$name"
echo "Wrote $out/$name.tar.gz"
