#!/usr/bin/env bash
# Dropout check of every Linux audio row in docs/benchmark.md, beside the live service
# (see beside.sh). Each row runs RUNS times, and every recording is kept and gap-checked.
#
# Usage: tools/measure/dropouts.sh [OUT=build/dropouts]
#   RUNS=3 SECONDS_PER_RUN=30
#   ONLY=mic-studio-voice   one row
set -uo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../.." && pwd)
python=${PYTHON:-python3}
out=${1:-$root/build/dropouts}
runs=${RUNS:-3} seconds=${SECONDS_PER_RUN:-30}
mkdir -p "$out"

row() {
    local name=$1 script=$2 var=$3 value=$4
    [[ -z ${ONLY:-} || $name == "$ONLY" ]] || return 0
    for i in $(seq "$runs"); do
        echo "=== $name run $i"
        env "$var=$value" KEEP="$out/$name-$i.f32" "$here/$script" "$seconds" 2>&1 |
            grep -v "MESA\|pci id"
        "$python" "$here/gapcheck.py" "$out/$name-$i.f32"
    done
}

row mic-noise-removal mic-latency.sh MIC_EFFECTS ''
row mic-studio-voice mic-latency.sh MIC_EFFECTS 'studio_voice = { enabled = true }'
row mic-no-effects mic-latency.sh MIC_EFFECTS 'noise_removal = { enabled = false }'
row speaker-noise-removal speaker-latency.sh SPEAKER_EFFECTS ''
row speaker-no-effects speaker-latency.sh SPEAKER_EFFECTS 'noise_removal = { enabled = false }'
