Two builds are attached:

- `broadcast-linux-*.pkg.tar.zst`: Arch package, built against the Wine in Arch's
  repositories at release time (on Arch, the AUR `PKGBUILD` builds against your own
  Wine instead).
- `broadcast-linux-*-x86_64.tar.gz`: for other distributions. Built on Debian 13
  (needs glibc 2.39 or newer, e.g. Ubuntu 24.04) against WineHQ's stable Wine
  11.0; the exact versions are in `BUILD-INFO` inside. Use Wine 11 (the relay does
  not even build against Wine 10); this build was tested on Wine 11.18. Unpack it
  and run `./install.sh`.

Both need an NVIDIA RTX GPU with the proprietary driver, PipeWire with WirePlumber,
ffmpeg, pactl, and v4l2loopback for the camera. NVIDIA's files are not included:
`broadcast-linux setup` downloads them from NVIDIA.
