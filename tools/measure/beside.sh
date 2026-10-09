# Sourced by the side-by-side tests. Runs a second broadcast-linux next to the live
# service: its own config and status socket, test nodes found by its process id, and
# everything it creates removed on exit, so the live service never has to stop.
# Needs: a finished `broadcast-linux setup`, ./build.sh (BIN= picks another binary,
# BROADCAST_LINUX_LIBDIR its workers), ffmpeg, pactl, and PYTHON (default python3)
# with numpy.
set -uo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
bin=${BIN:-$root/build/stage/bin/broadcast-linux}
python=${PYTHON:-python3}
sample="$root/afx-api/samples/effects_demo/input_files/denoiser/48k/Fan_48k.wav"
[[ -f $sample ]] || { echo "Missing $sample (clone NVIDIA-Maxine/Maxine-AFX-SDK into afx-api/)" >&2; exit 1; }
mkdir -p "$root/build"
# Recordings can be hundreds of MB, and /tmp may be RAM.
work=$(mktemp -d -p "$root/build" measure.XXXXXX)
modules=()
pids=()

cleanup() {
    [[ -n ${service:-} ]] && kill -TERM "$service" 2>/dev/null
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    sleep 2
    for m in "${modules[@]}"; do pactl unload-module "$m" 2>/dev/null; done
    [[ -n ${KEEP:-} && -f $work/recording.f32 ]] && cp "$work/recording.f32" "$KEEP"
    rm -rf "$work"
}
trap cleanup EXIT

sink() {
    modules+=("$(pactl load-module module-null-sink sink_name="$1")")
}

# Loops NVIDIA's sample into a sink.
play() {
    ffmpeg -nostdin -hide_banner -loglevel error -stream_loop -1 -re -i "$sample" \
        -f pulse -device "$1" "$1-player" &
    pids+=($!)
}

# Starts the test instance with config.toml text from $1.
start_service() {
    mkdir -p "$work/config/broadcast-linux" "$work/run"
    printf '%s\n' "$1" > "$work/config/broadcast-linux/config.toml"
    local runtime=$XDG_RUNTIME_DIR
    XDG_CONFIG_HOME="$work/config" XDG_RUNTIME_DIR="$work/run" PIPEWIRE_RUNTIME_DIR="$runtime" \
        PULSE_RUNTIME_PATH="$runtime/pulse" "$bin" run > "$work/service.log" 2>&1 &
    service=$!
    sleep 3
}

# Prints a property of the test instance's node $1, or of its port $3 with $2 = port.
own() {
    pw-dump | "$python" -c '
import json, sys
pid, node, what = sys.argv[1:4]
objs = json.load(sys.stdin)
props = lambda o: (o.get("info") or {}).get("props") or {}
clients = {o["id"] for o in objs if o["type"].endswith(":Client")
           and str(props(o).get("application.process.id")) == pid}
nodes = {o["id"]: props(o) for o in objs if o["type"].endswith(":Node")
         and props(o).get("node.name") == node and props(o).get("client.id") in clients}
if what == "port":
    for o in objs:
        p = props(o)
        if o["type"].endswith(":Port") and p.get("node.id") in nodes and p.get("port.name") == sys.argv[4]:
            print(o["id"])
else:
    for p in nodes.values():
        print(p.get(what, ""))' "$service" "$@"
}

# Forces the period of the test group with a silent player into sink $2.
force() {
    timeout 3600 pw-cat --playback --raw --format f32 --rate 48000 --channels 1 --target "$2" \
        -P "{ node.force-quantum = $1 }" - < /dev/zero &
    forcer=$!
    pids+=($forcer)
}

# Records sink $1's monitor as stereo f32 for $2 seconds, switching the forced period
# to FORCE_Q2 after FORCE_AT seconds when set.
record() {
    timeout "$2" parec -d "$1.monitor" --raw --format=float32le --channels=2 --rate=48000 \
        --latency-msec="${REC_MS:-10}" > "$work/recording.f32" &
    local recorder=$!
    if [[ -n ${FORCE_Q2:-} ]]; then
        sleep "${FORCE_AT:-8}"
        kill "$forcer"
        force "$FORCE_Q2" "$1"
    fi
    wait "$recorder"
}

report() {
    "$python" "$root/tools/measure/analyze.py" "$work/recording.f32" | head -1
    "$python" "$root/tools/measure/delays.py" "$work/recording.f32"
}

service_log() {
    grep -E "^$1" "$work/service.log" | grep -v "MESA\|pci id"
}
