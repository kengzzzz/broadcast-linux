#!/usr/bin/env bash
# Runs mic-latency.sh at forced periods, steady and switched mid-recording, for each
# binary in BINS (default: the ./build.sh one).
#
# Usage: [BINS="/usr/bin/broadcast-linux build/stage/bin/broadcast-linux"] tools/measure/period-matrix.sh
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../.." && pwd)
export MIC_EFFECTS=${MIC_EFFECTS:-'noise_removal = { enabled = true, strength = 1.0 }
room_echo_removal = { enabled = true, strength = 1.0 }'}

run() {
    local bin=$1 label=$2
    shift 2
    echo "$bin | $label: $(env BIN="$bin" REC_MS=100 "$@" "$here/mic-latency.sh" 20 2>&1 |
        grep -E '^delay per|^mic: stopped' | tr '\n' ' ')"
}

for bin in ${BINS:-$root/build/stage/bin/broadcast-linux}; do
    run "$bin" "512 steady" FORCE_Q=512
    run "$bin" "960 steady" FORCE_Q=960
    run "$bin" "256 to 960 at 8 s" FORCE_Q=256 FORCE_Q2=960 FORCE_AT=8
    run "$bin" "960 to 256 at 8 s" FORCE_Q=960 FORCE_Q2=256 FORCE_AT=8
done
