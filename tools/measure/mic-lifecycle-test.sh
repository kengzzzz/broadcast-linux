#!/usr/bin/env bash
# Mic lifecycle test: load on first use, pause after the idle timeout (real mic
# released, model kept), instant resume, unload after unload_after_minutes = 1.
# Same setup and requirements as mic-service-test.sh; stop the service first.
# Usage: [EXTRA='noise_removal = { enabled = false }'] tools/measure/mic-lifecycle-test.sh
set -uo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
python=${PYTHON:-python3}
work=$(mktemp -d)
sample="$root/afx-api/samples/effects_demo/input_files/denoiser/48k/Fan_48k.wav"
modules=()
cleanup() { [[ -n ${service:-} ]] && kill -TERM $service 2>/dev/null; [[ -n ${player:-} ]] && kill $player 2>/dev/null; sleep 1; for m in "${modules[@]}"; do pactl unload-module "$m" 2>/dev/null; done; rm -rf "$work"; }
trap cleanup EXIT
modules+=("$(pactl load-module module-null-sink sink_name=bltest_in)")
modules+=("$(pactl load-module module-remap-source source_name=bltest_src master=bltest_in.monitor)")
modules+=("$(pactl load-module module-null-sink sink_name=bltest_meas)")
ffmpeg -nostdin -loglevel error -stream_loop -1 -re -i "$sample" -f pulse -device bltest_in bltest & player=$!
mkdir -p $work/config/broadcast-linux
printf "[mic]\ninput = \"bltest_src\"\nunload_after_minutes = 1\n${EXTRA:-}\n[camera]\nenabled = false\n" > $work/config/broadcast-linux/config.toml
XDG_CONFIG_HOME=$work/config "${BIN:-$root/build/stage/bin/broadcast-linux}" run > $work/service.log 2>&1 & service=$!
sleep 2
st() { echo "$1: workers=$(pgrep -f 'afx_stream\.exe\.so' | wc -l) capture-links=$(pw-link -l | grep -c broadcast_linux_capture) bltest_src=$(pactl list short sources | awk '$2=="bltest_src"{print $NF}')"; }
st "t=0 idle, nothing loaded"
pw-link bltest_in:monitor_FL bltest_meas:playback_FL
pw-link broadcast_linux_mic:capture_MONO bltest_meas:playback_FR; sleep 6; st "first use"
pw-link -d broadcast_linux_mic:capture_MONO bltest_meas:playback_FR; sleep 8; st "8 s after last reader (expect paused: workers 1 with effects, links 0)"
pw-link broadcast_linux_mic:capture_MONO bltest_meas:playback_FR
timeout 20 parec -d bltest_meas.monitor --raw --format=float32le --channels=2 --rate=48000 --latency-msec=10 > $work/rec.f32
st "resumed"
"$python" $root/tools/measure/analyze.py $work/rec.f32
"$python" - $work/rec.f32 <<'PY'
import numpy as np, sys
x=np.fromfile(sys.argv[1],'<f4').reshape(-1,2)[:,1]; r=48000
nz=np.nonzero(np.abs(x)>1e-6)[0]
print(f"resume: first non-silent virtual-mic sample {nz[0]/r*1000:.0f} ms after recording started")
PY
pw-link -d broadcast_linux_mic:capture_MONO bltest_meas:playback_FR
sleep 75; st "75 s after last reader (expect unloaded: workers 0)"
grep -E "^mic: (started|paused|resumed|stopped|unloading|effect)" $work/service.log
