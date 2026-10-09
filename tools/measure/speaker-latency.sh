#!/usr/bin/env bash
# Virtual speaker delay and gaps, beside the live service (see beside.sh).
#
# Plays NVIDIA's Fan_48k.wav into a test sink, links its monitor into the test
# instance's virtual speaker, whose output goes to a second test sink, and records
# both on one stereo sink.
#
# Usage: tools/measure/speaker-latency.sh [SECONDS=30]
#   SPEAKER_EFFECTS='noise_removal = { enabled = false }'  extra [speaker] config
#   REC_MS=40               recorder latency, which sets the period
#   KEEP=file.f32           keep the recording
# The live service must not have its speaker enabled: both would be named
# broadcast_linux_speaker.
source "$(dirname -- "${BASH_SOURCE[0]}")/beside.sh"
seconds=${1:-30}

sink spk_in
sink spk_out
sink spk_meas
play spk_in
start_service "[mic]
enabled = false

[speaker]
enabled = true
output = \"spk_out\"
unload_after_minutes = 0
${SPEAKER_EFFECTS:-}

[camera]
enabled = false"

input=$(own broadcast_linux_speaker port playback_MONO)
[[ -n $input ]] || { echo "test speaker not found" >&2; cat "$work/service.log" >&2; exit 1; }
pw-link spk_in:monitor_FL spk_meas:playback_FL
pw-link spk_out:monitor_FL spk_meas:playback_FR
pw-link spk_in:monitor_FL "$input"
sleep 8
record spk_meas "$seconds"
report
pw-link -d spk_in:monitor_FL "$input"
sleep 8
service_log speaker
