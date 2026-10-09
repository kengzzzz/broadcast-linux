#!/usr/bin/env bash
# An app using the virtual mic the normal way, beside the live service (see beside.sh):
# pw-record targets the test mic and WirePlumber makes the link. Checks that the session
# starts and stops, that pipewire-pulse lists the mic, and that the recording has sound.
#
# Usage: tools/measure/mic-app-test.sh [SECONDS=20]
source "$(dirname -- "${BASH_SOURCE[0]}")/beside.sh"
seconds=${1:-20}

sink bltest_in
modules+=("$(pactl load-module module-remap-source source_name=bltest_src master=bltest_in.monitor)")
play bltest_in
start_service '[mic]
input = "bltest_src"
unload_after_minutes = 0

[speaker]
enabled = false

[camera]
enabled = false'

serial=$(own broadcast_linux_mic object.serial)
[[ -n $serial ]] || { echo "test mic not found" >&2; cat "$work/service.log" >&2; exit 1; }
echo "pipewire-pulse sources named broadcast_linux_mic: $(pactl list short sources | grep -c broadcast_linux_mic)"
timeout "$seconds" pw-record --target "$serial" --rate 48000 --channels 1 --format f32 "$work/app.wav"
"$python" -c '
import sys
import numpy as np
x = np.fromfile(sys.argv[1], "<f4")[1000:].astype(float)
print(f"recorded {len(x) / 48000:.1f} s at {20 * np.log10(np.sqrt(np.mean(x ** 2)) + 1e-12):.1f} dBFS")' \
    "$work/app.wav"
sleep 8
service_log mic
