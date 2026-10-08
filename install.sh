#!/usr/bin/env bash
#   curl -fsSL https://github.com/kengzzzz/broadcast-linux/releases/latest/download/install.sh | bash
# BROADCAST_LINUX_VERSION pins a release (0.4.1 or later, which have SHA256SUMS).
# BROADCAST_LINUX_URL replaces the download directory for testing; it needs a SHA256SUMS too.
set -euo pipefail

repo=https://github.com/kengzzzz/broadcast-linux

say() { echo "==> $*"; }
die() {
    echo "broadcast-linux: $*" >&2
    exit 1
}

has() { command -v "$1" >/dev/null; }

download() { curl -fL --retry 3 --connect-timeout 10 --speed-limit 1024 --speed-time 30 "$@"; }

latest_version() {
    local url
    url=$(download -sSI -o /dev/null -w '%{url_effective}' "$repo/releases/latest") ||
        die "could not reach $repo"
    [[ $url == */tag/v* ]] || die "could not find the latest release at $repo/releases"
    echo "${url##*/tag/v}"
}

resolve_release() {
    version=${BROADCAST_LINUX_VERSION:-$(latest_version)}
    base=${BROADCAST_LINUX_URL:-$repo/releases/download/v$version}
}

# apt reads the file as its _apt user.
fetch() {
    say "Downloading $1"
    download --progress-bar -o "$tmp/$1" "$base/$1" || die "download failed: $base/$1"
    download -sS -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
    awk -v f="$1" '$2 == f' "$tmp/SHA256SUMS" | (cd "$tmp" && sha256sum -c --quiet --strict) ||
        die "$1 does not match the release's SHA256SUMS"
    chmod 644 "$tmp/$1"
}

# Unless the kernel ships v4l2loopback, the camera needs a DKMS build, and neither Debian
# nor Arch installs kernel headers by default.
has_camera_module() { modinfo v4l2loopback >/dev/null 2>&1; }

load_camera_module() {
    if [[ ! -d /sys/module/v4l2loopback ]] && has_camera_module; then
        sudo modprobe v4l2loopback || true
    fi
}

arch_camera_packages() {
    has_camera_module && return
    local base headers=()
    while read -r base; do
        pacman -Si "$base-headers" >/dev/null 2>&1 && headers+=("$base-headers")
    done < <(sort -u /usr/lib/modules/*/pkgbase 2>/dev/null)
    if (( ${#headers[@]} == 0 )); then
        echo "Note: no headers package found for your kernel, so v4l2loopback-dkms was skipped." >&2
        return
    fi
    printf '%s\n' v4l2loopback-dkms "${headers[@]}"
}

install_arch() {
    local camera=()
    mapfile -t camera < <(arch_camera_packages)
    if has paru || has yay; then
        local helper
        helper=$(has paru && echo paru || echo yay)
        if (( ${#camera[@]} )); then
            sudo pacman -S --needed --noconfirm "${camera[@]}"
        fi
        say "Installing broadcast-linux-bin from the AUR with $helper"
        "$helper" -S --needed broadcast-linux-bin </dev/tty
    else
        say "Building broadcast-linux-bin from the AUR"
        sudo pacman -S --needed --noconfirm git base-devel "${camera[@]}"
        git clone --depth 1 https://aur.archlinux.org/broadcast-linux-bin.git "$tmp/aur"
        (cd "$tmp/aur" && makepkg -si </dev/tty)
    fi
    load_camera_module
}

# Headers for the kernel metapackages (linux-image-amd64 -> linux-headers-amd64) keep
# DKMS building after kernel updates.
deb_camera_headers() {
    has_camera_module && return
    [[ -e /lib/modules/$(uname -r)/build ]] && return
    local pkg
    for pkg in "linux-headers-$(uname -r)" $(dpkg-query -W -f '${db:Status-Abbrev}${Package}\n' 'linux-image-*' 2>/dev/null |
        sed -n 's/^ii *linux-image-\([a-z].*\)$/linux-headers-\1/p'); do
        apt-cache show "$pkg" >/dev/null 2>&1 && echo "$pkg"
    done
}

install_deb() {
    has apt-get || die "apt-get not found"
    sudo apt-get update
    local wine
    wine=$(apt-cache policy wine | sed -n 's/^ *Candidate: *//p')
    if [[ -z $wine || $wine == "(none)" ]] || ! dpkg --compare-versions "$wine" ge 10; then
        die "your distribution offers Wine ${wine:-(none)}, but broadcast-linux needs Wine 10 or newer
(Debian 13+, Ubuntu 26.04+ or Linux Mint based on it)"
    fi
    resolve_release
    fetch "broadcast-linux_${version}-1_amd64.deb"
    local headers=()
    mapfile -t headers < <(deb_camera_headers)
    sudo apt-get install -y "$tmp/broadcast-linux_${version}-1_amd64.deb" "${headers[@]}"
    load_camera_module
}

install_rpm() {
    has dnf || die "dnf not found"
    resolve_release
    fetch "broadcast-linux-${version}-1.x86_64.rpm"
    sudo dnf install -y "$tmp/broadcast-linux-${version}-1.x86_64.rpm"
    if ! rpm -q rpmfusion-free-release >/dev/null 2>&1; then
        echo "Note: RPM Fusion is not enabled, so the virtual camera's akmod-v4l2loopback was skipped."
        echo "      See https://rpmfusion.org/Configuration, then: sudo dnf install akmod-v4l2loopback"
    fi
}

install_tarball() {
    has wine || echo "Note: Wine 10 or newer is required; install it with your package manager."
    resolve_release
    fetch "broadcast-linux-${version}-x86_64.tar.gz"
    tar -C "$tmp" -xzf "$tmp/broadcast-linux-${version}-x86_64.tar.gz"
    "$tmp/broadcast-linux-$version-x86_64/install.sh"
}

# A tarball install's user unit in ~/.config overrides the package's and runs the old
# binary. Keep ~/.local/share/broadcast-linux: it holds the Wine prefix and NVIDIA's files.
remove_tarball_install() {
    local unit=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/broadcast-linux.service
    [[ -f $unit ]] || return 0
    local bin prefix
    bin=$(sed -n 's/^ExecStart=\([^ ]*\).*/\1/p' "$unit")
    [[ $bin == */bin/broadcast-linux && $bin != /usr/* ]] || return 0
    prefix=${bin%/bin/broadcast-linux}
    say "Removing the earlier tarball install in $prefix (settings and NVIDIA's files stay)"
    if systemctl --user -q is-active broadcast-linux 2>/dev/null; then
        restart=1
    fi
    if systemctl --user -q is-enabled broadcast-linux 2>/dev/null; then
        systemctl --user disable broadcast-linux
        reenable=1
    fi
    rm -f "$unit" "$prefix/bin/broadcast-linux" "$prefix/bin/broadcast-linux-gui" \
        "$prefix/share/applications/broadcast-linux.desktop" \
        "$prefix/share/icons/hicolor/scalable/apps/broadcast-linux.svg" \
        "$prefix/share/broadcast-linux/modules-load.conf" "$prefix/share/broadcast-linux/modprobe.conf"
    rm -rf "$prefix/lib/broadcast-linux"
    local f
    for f in /etc/modprobe.d/broadcast-linux.conf /etc/modules-load.d/broadcast-linux.conf; do
        if [[ -e $f ]]; then
            echo "Note: $f from the tarball setup overrides the package's copy; remove it with: sudo rm $f"
        fi
    done
}

restart_service() {
    systemctl --user daemon-reload 2>/dev/null || return 0
    if [[ -n ${reenable:-} ]]; then
        systemctl --user enable broadcast-linux
    fi
    if [[ -n ${restart:-} ]] || systemctl --user -q is-active broadcast-linux 2>/dev/null; then
        say "Restarting the broadcast-linux service"
        systemctl --user restart broadcast-linux || true
    fi
}

join_video_group() {
    local user
    user=$(id -un)
    if id -nG "$user" | tr ' ' '\n' | grep -qx video; then
        return
    fi
    say "Adding $user to the video group for the virtual camera"
    sudo usermod -aG video "$user"
    echo "Log out and back in for the camera to work."
}

main() {
    (( EUID != 0 )) || die "run this as your normal user; it asks for sudo when needed"
    [[ $(uname -m) == x86_64 ]] || die "only x86_64 is supported"
    has curl || die "curl not found"
    has sudo || die "sudo not found"
    # modinfo and modprobe live in sbin, which Debian leaves out of a user's PATH.
    PATH=$PATH:/usr/sbin:/sbin

    local id like
    # shellcheck disable=SC1091
    id=$(. /etc/os-release && echo "${ID:-}")
    # shellcheck disable=SC1091
    like=$(. /etc/os-release && echo "${ID_LIKE:-}")
    local ids=" $id $like "

    tmp=$(mktemp -d)
    chmod 755 "$tmp"
    trap 'rm -rf "$tmp"' EXIT

    local packaged=1
    if [[ -e /run/ostree-booted ]]; then
        install_tarball
        packaged=
    elif [[ $ids == *" arch "* ]]; then
        install_arch
    elif [[ $ids == *" debian "* || $ids == *" ubuntu "* ]]; then
        install_deb
    elif [[ $ids == *" fedora "* ]]; then
        install_rpm
    else
        install_tarball
        packaged=
    fi
    if [[ -n $packaged ]]; then
        remove_tarball_install
    fi
    restart_service
    join_video_group
    say "Done. Open broadcast-linux from your app menu, or run broadcast-linux-gui."
}

main "$@"
