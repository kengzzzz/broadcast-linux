#!/bin/sh
# dpkg passes "configure" and the old version (empty on a first install); rpm passes
# 1 on install and 2 on upgrade.
case "$1" in
    configure) [ -z "$2" ] && fresh=1 ;;
    1) fresh=1 ;;
    2) ;;
    *) exit 0 ;;
esac

# Without a reboot, modules-load.d only applies next boot.
modprobe v4l2loopback 2>/dev/null || true

if [ -z "${fresh:-}" ]; then
    echo "broadcast-linux: run 'systemctl --user restart broadcast-linux' to use the new version"
    exit 0
fi
cat <<'MSG'
broadcast-linux: open "broadcast-linux" from your app menu (or run broadcast-linux-gui)
to download NVIDIA's files, accept their licence, start the service and choose effects.
In a terminal instead, as your normal user:
  broadcast-linux setup
  systemctl --user enable --now broadcast-linux
For the camera, join the video group, then log in again:
  sudo usermod -aG video "$USER"
If your kernel lacks v4l2loopback, install it (Debian/Ubuntu: v4l2loopback-dkms,
Fedora: akmod-v4l2loopback from RPM Fusion), then run: sudo modprobe v4l2loopback
Check the setup: broadcast-linux doctor
MSG
