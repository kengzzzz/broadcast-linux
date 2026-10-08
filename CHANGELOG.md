# Changelog

## Unreleased

- Tarball: needs glibc 2.35+ (Ubuntu 22.04, Debian 12 and newer) and Wine 10+, down from glibc 2.39 and Wine 11. Distros that ship an older Wine need WineHQ stable.
- `pactl` is no longer needed: device lists, the default device and `doctor` ask PipeWire directly. `pipewire-pulse` is now optional, and `doctor` warns when it isn't running.
- `broadcast-linux devices` lists microphones and outputs with the node names the config uses.
- Packages: `.deb` for Debian 13+, Ubuntu 26.04+ and Linux Mint based on it, and `.rpm` for Fedora, both using the distribution's Wine.
- One install command for every distribution: `curl -fsSL https://github.com/kengzzzz/broadcast-linux/releases/latest/download/install.sh | bash`. It uses the AUR on Arch, the `.deb` or `.rpm` where they fit, the tarball elsewhere, adds you to the `video` group, and, when the camera module is missing, installs `v4l2loopback-dkms` with kernel headers on Arch and kernel headers on Debian-based systems. Rerunning it upgrades and restarts a running service. It replaces an earlier tarball install, keeping settings and NVIDIA's files.
- Releases include `SHA256SUMS`, which `install.sh` checks downloads against.

## 0.4.0 - 2026-10-07

A settings window, plus Eye Contact and Auto Frame for the camera.

- Settings window: open **broadcast-linux** from your app menu, or run `broadcast-linux-gui`. Choose devices and effects, preview the camera, run setup and check the system.
- Camera: Eye Contact and Auto Frame, off by default. Turn them on in the window, or with `eye_contact` and `auto_frame` in the config.
- Camera: apps that keep the camera open during a service restart get the effects back.
- Camera: a new background image always replaces the previous one.
- Camera: fixing the webcam settings after a failed start works without reopening the app.
- Camera: changing the resolution while an app uses the camera shows a clear error instead of a garbled picture.
- Service: a second copy refuses to start instead of creating duplicate devices.
- Setup: every GPU's installer is checked against a pinned checksum. `--allow-unverified` is no longer needed.
- Setup: checks free disk space before downloading.
- Setup: finds missing or damaged NVIDIA files and installs them again.
- Setup: running it again no longer waits for effects in use to stop.
- `broadcast-linux doctor` prints fix commands on their own lines, including module setup for tarball installs, and shows paths in your home as `~`.
- Tarball: `install.sh` also installs the app menu entry, its icon and the module config files.
- Packages now require `pipewire-pulse`.
- Docs: configuration, CLI and build guides are in `docs/`.

## 0.3.1 - 2026-10-06

Lighter virtual camera, and ffmpeg is no longer needed.

- Virtual camera: much lower CPU usage and slightly lower latency.
- Virtual camera: damaged webcam frames are skipped instead of shown.
- Virtual camera: webcam streams above 1080p30 decode on several CPU cores; `parallel_decode` turns this on or off.
- ffmpeg is no longer needed.
- Building from source needs cmake and nasm; CI and release builds updated.

## 0.3.0 - 2026-10-05

Virtual speaker, a setup checker, and webcams without MJPEG.

- Virtual speaker (off by default): noise and room echo removal for call audio.
- `broadcast-linux doctor` checks the setup and prints what to fix.
- Virtual camera: webcams without MJPEG work; `input_format` defaults to `auto`.
- Virtual camera: startup errors name the fix (module not loaded, wrong device path, missing `video` group).
- Release tarball and Arch packages include this changelog.

## 0.2.1 - 2026-10-02

Build fixes; the program is unchanged.

- Rust version pinned for local, CI and release builds.
- `build.sh` runs from any directory.

## 0.2.0 - 2026-10-02

Background blur and removal, and an AUR binary package.

- Virtual camera: background blur and background removal (black).
- Only one background effect at a time; configs with more are rejected.
- Arch: `broadcast-linux-bin` installs the release tarball. Wine 11+ required.
- Clearer install steps and example config.

## 0.1.0 - 2026-09-30

First release.

- Virtual mic: noise removal, room echo removal and Studio Voice.
- Virtual camera: video noise removal, background replacement and Studio Light.
- `broadcast-linux setup` downloads NVIDIA's runtime and models for your GPU.
