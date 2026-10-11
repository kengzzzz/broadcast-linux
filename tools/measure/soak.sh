#!/usr/bin/env bash
# Dropout soak beside the live service, about 3 hours (see beside.sh): long mic and speaker
# runs, alone, together, beside the camera effects and under full CPU load, then
# mic-reopen.sh. Each recording is kept and gap-checked. PipeWire's global forced period
# and rate are logged every 5 s, since a period change can cause a gap.
#
# Usage: tools/measure/soak.sh [OUT=build/soak]
#   LONG=1800 SHORT=600     run lengths in seconds (SHORT is the CPU load run)
#   REOPEN_CYCLES=20 REOPEN_SECONDS=30
#   FRAMES=file.bgr         camera frames for tools/dev/camera-load.sh
set -uo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../.." && pwd)
python=${PYTHON:-python3}
out=${1:-$root/build/soak}
long=${LONG:-1800} short=${SHORT:-600}
mkdir -p "$out"

(while :; do
    printf '%s %s\n' "$(date +%T)" "$(pw-metadata -n settings 0 2>/dev/null | grep -o "clock\.force-[a-z]*' value:'[0-9]*'" | tr '\n' ' ')"
    sleep 5
done) > "$out/force-settings.log" &
watcher=$!
trap 'kill $watcher 2>/dev/null' EXIT

measure() {
    local name=$1 script=$2 seconds=$3
    shift 3
    echo "$(date +%T) start $name"
    env "$@" KEEP="$out/$name.f32" "$here/$script" "$seconds" > "$out/$name.log" 2>&1
    "$python" "$here/gapcheck.py" "$out/$name.f32" >> "$out/$name.log" 2>&1
    echo "$(date +%T) done $name"
}

measure mic-noise-removal mic-latency.sh "$long"
measure mic-studio-voice mic-latency.sh "$long" MIC_EFFECTS='studio_voice = { enabled = true }'
measure speaker-noise-removal speaker-latency.sh "$long"

"$root/tools/dev/camera-load.sh" $((long + 60)) ${FRAMES:+"$FRAMES"} > "$out/camera-load.log" 2>&1 &
camera=$!
sleep 20
measure mic-with-camera mic-latency.sh "$long"
wait "$camera"

measure mic-beside-speaker mic-latency.sh "$long" &
mic=$!
measure speaker-beside-mic speaker-latency.sh "$long"
wait "$mic"

echo "$(date +%T) start mic-reopen"
"$here/mic-reopen.sh" "${REOPEN_CYCLES:-20}" "${REOPEN_SECONDS:-30}" > "$out/mic-reopen.log" 2>&1
echo "$(date +%T) done mic-reopen"

timeout $((short + 120)) python3 -I "$root/tools/dev/cpu-load.py" $((short + 60)) &
load=$!
sleep 10
measure mic-with-cpu-load mic-latency.sh "$short"
wait "$load"
echo "$(date +%T) all done"
