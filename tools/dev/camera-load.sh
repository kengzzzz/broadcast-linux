#!/usr/bin/env bash
# Runs the camera worker offline for SECONDS on recorded raw BGR24 1080p frames, looped
# at 30 fps with denoise and Studio Light, as GPU load for audio tests. The webcam and
# the virtual camera are not used.
# Usage: tools/dev/camera-load.sh SECONDS [FRAMES.bgr=build/camera-test/cam1080.bgr]
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$here/env.sh"
frames=$(realpath -- "${2:-$repo/build/camera-test/cam1080.bgr}")
cd "$runtime"
python3 -I "$here/loop-frames.py" "$frames" "$1" |
    env "${wine_env[@]}" wine "$libdir/workers/camera_stream.exe.so" \
        "$(winpath "$(newest nvbcast_vfx_gs_v0_9)")" --size 1920x1080 \
        --denoise "$(winpath "$(newest nvbcast_vfx_lld_v0_9)")" \
        --relight "$(winpath "$(newest nvbcast_vfx_rl_v0_9)")" \
        --hdr "$nvidia/studio_light/vkl_mid.hdr" > /dev/null 2> "$out/camera-load.log"
grep -v "pci id\|MESA" "$out/camera-load.log" | tail -1
