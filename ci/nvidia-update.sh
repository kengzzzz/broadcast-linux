#!/usr/bin/env bash
# Checks NVIDIA's Broadcast installers against src/setup.rs, and writes an issue to
# REPORT when a newer build is out. PINNED_BUILD fakes an older pinned build.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
report=$1
page=https://www.nvidia.com/en-us/geforce/broadcasting/broadcast-app/
host=https://international.download.nvidia.com/Windows/broadcast
repo=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-kengzzzz/broadcast-linux}

installers=$(sed -n '/^pub const INSTALLERS/,/^];/p' "$root/src/setup.rs" \
    | grep -oE 'Generation::[A-Za-z]+|url: "[^"]+"|size: [0-9_]+|sha256: "[0-9a-f]+"' \
    | sed -E 's/^Generation:://; s/^url: "(.*)"/\1/; /^size: /s/_//g; s/^size: //; s/^sha256: "(.*)"/\1/' \
    | paste - - - -)
pinned=$(sed -n 's/^pub const BUILD: &str = "\(.*\)";/\1/p' "$root/src/setup.rs")
[[ -n $installers && -n $pinned ]] || { echo "::error::cannot read the installers from src/setup.rs"; exit 1; }

length() {
    local n
    n=$(curl -sfIL --retry 3 -o /dev/null -w '%header{content-length}' "$1") || return 1
    echo "$n"
}

offline_url() {
    local build=$1 generation=$2
    echo "$host/${build%.*}/NVIDIA_Broadcast_Offline_${generation}_v$build.exe"
}

broken=""
while read -r generation url size _; do
    [[ $url == "$(offline_url "$pinned" "$generation")" ]] \
        || echo "::warning::$generation URL does not follow NVIDIA's usual pattern: $url"
    actual=$(length "$url" || true)
    if [[ $actual != "$size" ]]; then
        echo "::error::$generation installer $url is ${actual:-unavailable}, expected $size bytes"
        broken+="- $generation: $url is ${actual:-unavailable}, expected $size bytes"$'\n'
    fi
done <<< "$installers"
[[ -n $broken ]] || echo "Pinned NVIDIA Broadcast $pinned installers are available"

html=$(curl -sfL --retry 3 -A "Mozilla/5.0" "$page" || true)
latest=$(grep -oE "$host/[0-9.]+/NVIDIA_Broadcast_v[0-9.]+\.exe" <<< "$html" \
    | sed -nE '1s/.*_v([0-9.]+)\.exe/\1/p' || true)
if [[ -z $latest ]]; then
    echo "::error::no NVIDIA Broadcast installer link found on $page"
    exit 1
fi
pinned=${PINNED_BUILD:-$pinned}
if [[ $latest == "$pinned" ]]; then
    echo "NVIDIA Broadcast $latest is the latest build"
    [[ -z $broken ]]
    exit
fi
echo "NVIDIA Broadcast $latest is available (pinned: $pinned)"

sevenzip=$(command -v 7zz || command -v 7z) || { echo "::error::7-Zip is needed to list the installers"; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mapfile -t folders < <(grep -ohE '"nvbcast_[a-z_]+_v[0-9_]+"' "$root/src/nvidia.rs" "$root/src/config.rs" | tr -d '"' | sort -u)
mapfile -t models < <(grep -ohE '"[A-Za-z0-9_]+\.trtpkg"' "$root/src/config.rs" | tr -d '"' | sort -u)
mapfile -t presets < <(grep -ohE '"vkl_[a-z0-9_]+\.hdr"' "$root/src/config.rs" | tr -d '"' | sort -u)

entries=""
problems=""
expect() {
    grep -qE "$1" "$work/files" || problems+="- $generation: no \`$2\`"$'\n'
}
while read -r generation _; do
    url=$(offline_url "$latest" "$generation")
    exe=$work/installer.exe
    if ! curl -sfL --retry 3 -o "$exe" "$url"; then
        problems+="- $generation: no installer at $url"$'\n'
        continue
    fi
    size=$(stat -c %s "$exe")
    sha=$(sha256sum "$exe" | cut -d' ' -f1)
    "$sevenzip" l -slt "$exe" < /dev/null | sed -n 's/^Path = //p' > "$work/files"
    rm "$exe"
    expect '^NvMaxineModels/NvModels/nvbcast/versions/[^/]+/files/[^/]+/NVAudioEffects\.dll$' NVAudioEffects.dll
    for folder in "${folders[@]}"; do
        expect "^NvMaxineModels/NvModels/$folder/versions/[^/]+/files/[^/]+/" "$folder"
    done
    for model in "${models[@]}"; do
        expect "^NvMaxineModels/NvModels/[^/]+/versions/[^/]+/files/[^/]+/${model//./\\.}$" "$model"
    done
    for preset in "${presets[@]}"; do
        expect "^NvMaxineClient/nv/${preset//./\\.}$" "$preset"
    done
    entries+="    Installer {
        generation: Generation::$generation,
        build: BUILD,
        url: \"$url\",
        size: $(sed -E ':a; s/([0-9])([0-9]{3})($|_)/\1_\2\3/; ta' <<< "$size"),
        sha256: \"$sha\",
    },
"
    echo "Checked $generation: $size bytes"
done <<< "$installers"

{
    echo "NVIDIA Broadcast $latest is available"
    echo
    echo "NVIDIA's page now offers build \`$latest\`. broadcast-linux pins \`$pinned\`."
    echo
    if [[ -n $broken ]]; then
        echo "The pinned installers have changed, so setup fails for new users:"
        echo
        printf '%s' "$broken"
        echo
    fi
    echo "### Installer entries for \`src/setup.rs\`"
    echo
    echo '```rust'
    echo "pub const BUILD: &str = \"$latest\";"
    echo
    printf '%s' "$entries"
    echo '```'
    echo
    echo "### Files the code expects"
    echo
    if [[ -n $problems ]]; then
        printf '%s' "$problems"
    else
        echo "Every installer has the model folders, audio models and Studio Light presets the code names."
    fi
    echo
    echo "Follow [Updating NVIDIA Broadcast]($repo/blob/main/docs/building.md#updating-nvidia-broadcast) and test every effect on a GPU before releasing."
} > "$report"
[[ -z $broken ]]
