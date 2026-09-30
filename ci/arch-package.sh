#!/usr/bin/env bash
# Builds the Arch package from this checkout's HEAD with the real PKGBUILD, inside an
# archlinux:base-devel container (run as root). Writes the package to OUT.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=$(realpath -m -- "$1")

pacman -Syu --noconfirm --needed git rust clang meson ninja wine libpipewire
id builder &>/dev/null || useradd -m builder
git config --system --add safe.directory '*'

work=$(mktemp -d)
cp "$root/packaging/PKGBUILD" "$root/packaging/broadcast-linux.install" "$work/"
commit=$(git -C "$root" rev-parse HEAD)
sed -i "s|\"git+\$url.git#tag=v\$pkgver\"|\"broadcast-linux::git+file://$root#commit=$commit\"|" "$work/PKGBUILD"
grep -q "git+file://" "$work/PKGBUILD"
chown -R builder "$work"
su builder -c "cd '$work' && makepkg --nodeps --noconfirm"
mkdir -p "$out"
cp "$work"/*.pkg.tar.zst "$out/"
