#!/usr/bin/env bash
# Runs the audio worker offline: feeds INPUT (any format ffmpeg reads) in real time
# and writes $OUT/NAME.wav. STRENGTH defaults to 1.0.
# Usage: tools/dev/audio-worker.sh INPUT NAME EFFECT [EFFECT...]
set -euo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/env.sh"
in=$1 name=$2; shift 2
args=()
for effect in "$@"; do
    case $effect in
        denoiser) model=nvbcast_afx_bnr_v0_9 file=denoiser_48k.trtpkg ;;
        dereverb) model=nvbcast_afx_rec_v0_9 file=dereverb_48k.trtpkg ;;
        dereverb_denoiser) model=nvbcast_afx_bnrrec_v0_9 file=dereverb_denoiser_48k.trtpkg ;;
        studio_voice_low_latency) model=nvbcast_afx_stdvoice_v0_9 file=studio_voice_low_latency_48k.trtpkg ;;
        *) echo "unknown effect $effect" >&2; exit 2 ;;
    esac
    args+=("$effect" "$(winpath "$(newest $model)$file")" "${STRENGTH:-1.0}")
done
ffmpeg -nostdin -loglevel error -y -i "$in" -ac 1 -ar 48000 -f f32le "$out/$name.in.f32"
cd "$runtime"
python3 "$repo/tools/dev/pace.py" "$out/$name.in.f32" \
    | env "${wine_env[@]}" wine "$libdir/workers/afx_stream.exe.so" 0 "${args[@]}" \
        2> "$out/$name.log" > "$out/$name.f32"
ffmpeg -nostdin -loglevel error -y -f f32le -ar 48000 -ac 1 -i "$out/$name.f32" "$out/$name.wav"
rm -f "$out/$name.in.f32" "$out/$name.f32"
grep -v "pci id\|MESA" "$out/$name.log"
