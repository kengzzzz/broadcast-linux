# Changelog

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
