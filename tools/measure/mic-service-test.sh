#!/usr/bin/env bash
# End-to-end test of the virtual mic without a real microphone.
#
# Plays NVIDIA's Fan_48k.wav sample on a loop into a temporary source, runs
# `broadcast-linux run` with that source as its input, links the virtual mic and
# the original into one stereo sink (same clock), records it, and analyses delay,
# noise reduction and gaps. Also checks idle, on-demand start and idle stop.
#
# Usage: [MIC_EFFECTS='studio_voice = {}'] tools/measure/mic-service-test.sh [SECONDS=40]
# MIC_EFFECTS is extra [mic] config (default: noise removal only). Stop the
# broadcast-linux service first; both would use the same node name.
# Needs: a finished `broadcast-linux setup` (XDG_DATA_HOME honoured), ./build.sh
# (BIN= picks another binary), ffmpeg, and PYTHON (default python3) with numpy.
set -uo pipefail

seconds=${1:-40}
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
bin=${BIN:-$root/build/stage/bin/broadcast-linux}
work=$(mktemp -d)
python=${PYTHON:-python3}
sample="$root/afx-api/samples/effects_demo/input_files/denoiser/48k/Fan_48k.wav"
[[ -f $sample ]] || { echo "Missing $sample (clone NVIDIA-Maxine/Maxine-AFX-SDK into afx-api/)" >&2; exit 1; }

modules=()
cleanup() {
    [[ -n ${service:-} ]] && kill -TERM "$service" 2>/dev/null
    [[ -n ${player:-} ]] && kill "$player" 2>/dev/null
    sleep 1
    for m in "${modules[@]}"; do pactl unload-module "$m" 2>/dev/null; done
    rm -rf "$work"
}
trap cleanup EXIT

modules+=("$(pactl load-module module-null-sink sink_name=bltest_in)")
modules+=("$(pactl load-module module-remap-source source_name=bltest_src master=bltest_in.monitor)")
modules+=("$(pactl load-module module-null-sink sink_name=bltest_meas)")
ffmpeg -nostdin -hide_banner -loglevel error -stream_loop -1 -re -i "$sample" -f pulse -device bltest_in bltest &
player=$!

mkdir -p "$work/config/broadcast-linux"
printf '[mic]\ninput = "bltest_src"\n%s\n\n[camera]\nenabled = false\n' "${MIC_EFFECTS:-}" > "$work/config/broadcast-linux/config.toml"
XDG_CONFIG_HOME="$work/config" \
    "$bin" run > "$work/service.log" 2>&1 &
service=$!
sleep 2

workers() { pgrep -f 'afx_stream\.exe\.so' | wc -l; }
captures() { pw-link -l | grep -c 'broadcast_linux_capture'; }
echo "idle:            workers=$(workers) capture links=$(captures)   (expect 0 0)"

pw-link bltest_in:monitor_FL bltest_meas:playback_FL
pw-link broadcast_linux_mic:capture_MONO bltest_meas:playback_FR
sleep 8
echo "reader linked:   workers=$(workers) capture links=$(captures)   (expect 1, >0)"
timeout "$seconds" parec -d bltest_meas.monitor --raw --format=float32le --channels=2 --rate=48000 \
    --latency-msec=10 > "$work/recording.f32"
"$python" "$root/tools/measure/analyze.py" "$work/recording.f32"

pw-link -d broadcast_linux_mic:capture_MONO bltest_meas:playback_FR
sleep 8
echo "after idle stop: workers=$(workers) capture links=$(captures)   (expect 0 0)"
grep -E '^mic:' "$work/service.log"
