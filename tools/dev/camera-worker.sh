#!/usr/bin/env bash
# Runs the camera worker offline on raw BGR24 frames and saves the last YUYV output frame
# as $OUT/NAME.png. In the worker arguments @DN, @RL and @PRESET:<name> expand to
# the denoise and relighting model folders and a Studio Light preset file.
# Usage: SIZE=1920x1080 tools/dev/camera-worker.sh FRAMES.bgr NAME [worker args...]
#   e.g. ... frames.bgr all --denoise @DN --relight @RL --hdr @PRESET:vkl_mid.hdr
set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/env.sh"
in=$(realpath -- "$1") name=$2; shift 2
size=${SIZE:-1920x1080}
args=()
for a in "$@"; do
    case $a in
        @DN) a=$(winpath "$(newest nvbcast_vfx_lld_v0_9)") ;;
        @RL) a=$(winpath "$(newest nvbcast_vfx_rl_v0_9)") ;;
        @PRESET:*) a="$nvidia/studio_light/${a#@PRESET:}" ;;
    esac
    args+=("$a")
done
cd "$runtime"
env "${wine_env[@]}" wine "$libdir/workers/camera_stream.exe.so" \
    "$(winpath "$(newest nvbcast_vfx_gs_v0_9)")" --size "$size" "${args[@]}" \
    < "$in" > "$out/$name.yuyv" 2> "$out/$name.log"
grep -v "pci id\|MESA" "$out/$name.log" | tail -3
w=${size%x*} h=${size#*x}
frames=$(( $(stat -c %s "$out/$name.yuyv") / (w * h * 2) ))
if (( frames > 0 )); then
    ffmpeg -nostdin -loglevel error -y -f rawvideo -pix_fmt yuyv422 -video_size "$size" \
        -i "$out/$name.yuyv" -vf "select=eq(n\,$((frames - 1)))" -frames:v 1 "$out/$name.png"
fi
rm -f "$out/$name.yuyv"
