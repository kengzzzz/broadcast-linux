#!/usr/bin/env bash
# An app reopening the virtual mic, beside the live service (see beside.sh). One test
# instance keeps the noise removal model loaded. The app (a link to the recorder) opens
# the mic, records, then closes it for long enough that the mic pauses and logs its
# counters. Each cycle's recording is gap-checked.
#
# Usage: tools/measure/mic-reopen.sh [CYCLES=20] [SECONDS=30]
source "$(dirname -- "${BASH_SOURCE[0]}")/beside.sh"
cycles=${1:-20} seconds=${2:-30}

sink bltest_in
modules+=("$(pactl load-module module-remap-source source_name=bltest_src master=bltest_in.monitor)")
sink bltest_meas
play bltest_in
start_service "[mic]
input = \"bltest_src\"
unload_after_minutes = 5

[speaker]
enabled = false

[camera]
enabled = false"

port=$(own broadcast_linux_mic port capture_MONO)
[[ -n $port ]] || { echo "test mic not found" >&2; cat "$work/service.log" >&2; exit 1; }
pw-link bltest_in:monitor_FL bltest_meas:playback_FL
for cycle in $(seq "$cycles"); do
    pw-link "$port" bltest_meas:playback_FR
    sleep 3
    record bltest_meas "$seconds"
    echo "-- cycle $cycle: $("$python" "$root/tools/measure/analyze.py" "$work/recording.f32" | head -1)"
    "$python" "$root/tools/measure/gapcheck.py" "$work/recording.f32"
    pw-link -d "$port" bltest_meas:playback_FR
    sleep 8
done
service_log mic
