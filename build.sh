#!/usr/bin/env bash
# Builds broadcast-linux into OUT (default build/stage):
#   OUT/bin/broadcast-linux, OUT/bin/broadcast-linux-gui
#   OUT/lib/broadcast-linux/wine/     the patched nvcuda relay
#   OUT/lib/broadcast-linux/workers/  the Wine worker programs
# Needs the wine-nvcuda and vfx-api submodules, cargo, cmake and nasm (for libjpeg-turbo),
# Wine's winegcc and headers, meson, ninja and mingw-w64. Extra cargo flags can be passed
# in CARGO_ARGS.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
out=$(realpath -m -- "${1:-$root/build/stage}")
lib="$out/lib/broadcast-linux"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$root"

for sub in wine-nvcuda vfx-api; do
    if [[ -z $(ls -A "$root/$sub" 2>/dev/null) ]]; then
        echo "Submodule $sub is missing: git submodule update --init" >&2
        exit 1
    fi
done

# shellcheck disable=SC2086
cargo build --release --workspace --manifest-path "$root/Cargo.toml" ${CARGO_ARGS:-}
install -Dm755 -t "$out/bin" "$root/target/release/broadcast-linux" "$root/target/release/broadcast-linux-gui"

cp -r "$root/wine-nvcuda" "$work/relay-src"
rm -rf "$work/relay-src/.git"
patch -d "$work/relay-src" -p1 --quiet < "$root/patches/nvcuda-export-table-d2688bf2.patch"
"$work/relay-src/package-release.sh" relay "$work/relay" --fakedll > "$work/relay.log" 2>&1 || {
    tail -40 "$work/relay.log" >&2
    exit 1
}
rm -rf "$lib/wine"
mkdir -p "$lib"
cp -r "$work/relay/nvcuda-relay/lib/wine" "$lib/wine"

mkdir -p "$lib/workers"
winegcc -m64 -Wno-attributes -o "$lib/workers/afx_stream.exe" "$root/workers/afx_stream.c"
winegcc -m64 -Wno-attributes -I "$root/vfx-api/nvvfx/include" \
    -o "$lib/workers/camera_stream.exe" "$root/workers/camera_stream.c" -lm
echo "Built $out"
