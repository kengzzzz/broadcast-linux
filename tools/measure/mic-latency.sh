#!/usr/bin/env bash
# Virtual mic delay and gaps, beside the live service (see beside.sh).
#
# Loops NVIDIA's Fan_48k.wav into a test source, links the test instance's virtual mic
# and the original into one stereo sink, records it and prints the delay (overall and
# per 5 s) and the service's gap counters.
#
# Usage: tools/measure/mic-latency.sh [SECONDS=30]
#   MIC_EFFECTS='studio_voice = { enabled = true }'  extra [mic] config (default: noise removal)
#   INPUT=<node> REF=<node>:capture_FL  a real mic instead, recorded against itself
#   REC_MS=40               recorder latency, which sets the period (10 ms gives 256, 40 gives 512)
#   FORCE_Q=960             force the test group's period, FORCE_Q2=256 FORCE_AT=8 to switch it
#   UNLOAD=5 CYCLES=3       keep the model loaded, and link, record and unlink 3 times
#   KEEP=file.f32           keep the last recording
source "$(dirname -- "${BASH_SOURCE[0]}")/beside.sh"
seconds=${1:-30}

sink bltest_in
modules+=("$(pactl load-module module-remap-source source_name=bltest_src master=bltest_in.monitor)")
sink bltest_meas
play bltest_in
start_service "[mic]
input = \"${INPUT:-bltest_src}\"
unload_after_minutes = ${UNLOAD:-0}
${MIC_EFFECTS:-}

[speaker]
enabled = false

[camera]
enabled = false"

port=$(own broadcast_linux_mic port capture_MONO)
[[ -n $port ]] || { echo "test mic not found" >&2; cat "$work/service.log" >&2; exit 1; }
pw-link "${REF:-bltest_in:monitor_FL}" bltest_meas:playback_FL
[[ -n ${FORCE_Q:-} ]] && force "$FORCE_Q" bltest_meas
for cycle in $(seq "${CYCLES:-1}"); do
    pw-link "$port" bltest_meas:playback_FR
    sleep 8
    record bltest_meas "$seconds"
    echo "-- cycle $cycle"
    report
    pw-link -d "$port" bltest_meas:playback_FR
    sleep 8
done
service_log mic
