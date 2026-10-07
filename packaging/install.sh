#!/usr/bin/env bash
# Installs this release for the current user (default PREFIX: ~/.local) and sets up
# the systemd user unit. Steps that need root are printed, not run.
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
prefix=$(realpath -m -- "${PREFIX:-$HOME/.local}")
config_home=${XDG_CONFIG_HOME:-$HOME/.config}

install -Dm755 -t "$prefix/bin" "$here/bin/broadcast-linux" "$here/bin/broadcast-linux-gui"
rm -rf "$prefix/lib/broadcast-linux"
mkdir -p "$prefix/lib"
cp -r "$here/lib/broadcast-linux" "$prefix/lib/broadcast-linux"
install -Dm644 -t "$prefix/share/broadcast-linux" "$here/share/modules-load.conf" "$here/share/modprobe.conf"
install -Dm644 "$here/share/broadcast-linux.svg" "$prefix/share/icons/hicolor/scalable/apps/broadcast-linux.svg"
# Launchers often lack ~/.local/bin in PATH, so the menu entry uses the full path.
desktop="$prefix/share/applications/broadcast-linux.desktop"
mkdir -p "$(dirname "$desktop")"
sed "s|^Exec=.*|Exec=$prefix/bin/broadcast-linux-gui|" "$here/share/broadcast-linux.desktop" > "$desktop"

unit="$config_home/systemd/user/broadcast-linux.service"
mkdir -p "$(dirname "$unit")"
sed "s|^ExecStart=.*|ExecStart=$prefix/bin/broadcast-linux run|" "$here/share/broadcast-linux.service" > "$unit"

config="$config_home/broadcast-linux/config.toml"
if [[ ! -e $config ]]; then
    install -Dm644 "$here/share/config.toml" "$config"
fi
systemctl --user daemon-reload

cat <<MSG
Installed to $prefix (unit: $unit, config: $config).

Open "broadcast-linux" from your app menu (or run broadcast-linux-gui) to download
NVIDIA's files, start the service and choose effects. In a terminal instead:
  1. broadcast-linux setup                    # downloads NVIDIA Broadcast for your GPU
  2. For the virtual camera, as root:
       install -Dm644 $prefix/share/broadcast-linux/modules-load.conf /etc/modules-load.d/broadcast-linux.conf
       install -Dm644 $prefix/share/broadcast-linux/modprobe.conf /etc/modprobe.d/broadcast-linux.conf
       modprobe v4l2loopback
       usermod -aG video $USER                # then log in again
  3. systemctl --user enable --now broadcast-linux
  4. broadcast-linux doctor                   # checks the setup
MSG
if [[ :$PATH: != *":$prefix/bin:"* ]]; then
    echo "Note: $prefix/bin is not in your PATH."
fi
