# Sourced by the worker harnesses: locates the NVIDIA runtime, models and libdir.
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}/broadcast-linux
libdir=${BROADCAST_LINUX_LIBDIR:-$repo/build/lib}
nvidia=$(ls -d "$data"/nvidia/*/ | sort | tail -1)
newest() { ls -d "$nvidia/models/$1"/versions/*/files/*/ | sort | tail -1; }
winpath() { local p="Z:${1%/}"; printf '%s\n' "${p//\//\\}"; }
runtime=$(newest nvbcast)
out=${OUT:-$repo/build/dev-out}
mkdir -p "$out"
wine_env=(WINEPREFIX="$data/prefix" WINEDLLPATH="$libdir/workers:$libdir/wine"
    WINEDLLOVERRIDES="nvcuda=b;nvapi64=n;dxgi=n;d3d11=n" DXVK_ENABLE_NVAPI=1
    DXVK_LOG_LEVEL=none DXVK_NVAPI_LOG_LEVEL=none WINEDEBUG=-all)
