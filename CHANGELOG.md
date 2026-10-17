# Changelog

## Unreleased

NVIDIA Broadcast 2.3.0, with its mic profiles, RTX Video Super Resolution, Frame Generation and File Processor.

- Setup: uses NVIDIA Broadcast 2.3.0. Run `broadcast-linux setup` after upgrading. It downloads about 2.4 GB, asks you to accept NVIDIA's licence again and then removes the 2.2.1 files. Until then the service keeps using 2.2.1.
- Camera: background blur is now done by broadcast-linux, because Broadcast 2.3.0 no longer ships NVIDIA's blur. It is tuned to match NVIDIA's look, and also works with the 2.2.1 files.
- Camera: Eye Contact is about twice as fast in our tests (RTX 5080, 1080p).
- Settings window: a new layout like NVIDIA Broadcast on Windows. Audio, Video and Settings pages, effects with switches and their options under each, and the camera preview beside the effects. Narrow windows show one column, and it follows the system's light or dark theme.
- Mic and speaker: the log names the apps reading them, for example `mic: 1 app(s) reading (vesktop)`.
- Mic: Studio Voice mic profiles Bright, Full and Warm, as on Windows (`studio_voice.profile`). A profile adds about 30 ms of delay.
- Camera: RTX Video Super Resolution (`super_resolution`). It captures the webcam at a smaller size and upscales it with AI. Needs an RTX 30 series or newer GPU.
- Camera: RTX Video Frame Generation (`frame_generation`). It runs the webcam at a lower frame rate and makes the frames in between with AI, for example 60 fps from a 30 fps webcam. Needs an RTX 40 series or newer GPU.
- New File Processor: runs the effects on video and audio files, on the settings window's new Files page or with `broadcast-linux process`. It needs ffmpeg, which the packages now suggest.
- Settings window: a mic profile picker, and Super Resolution and Frame Generation rows that say why they are greyed out on GPUs without them.
- `doctor`: new `rtx video` and `ffmpeg` items.

## 0.4.4 - 2026-10-12

Lower camera delay on slower GPUs, and lower mic and speaker delay with fewer short gaps.

- Camera: less delay when the GPU can't keep up with the webcam. Each frame is decoded just before the GPU is free, instead of waiting in line.
- Mic and speaker: 2.7 ms less delay with noise or echo removal.
- Mic and speaker: no short gap when an app reopens them. The model runs on silence until it unloads, so the GPU doesn't idle.
- Mic and speaker: `unload_after_minutes` defaults to 2, since the GPU stays awake until then. Existing configs keep their value, often the old default of 10.
- Mic and speaker: no short gap while the GPU is in a slower power state. The delay is 5.3 ms longer for the first 10 s and while the GPU is slow or busy, for example with the camera effects on.
- Mic: no more short gaps every few minutes with Studio Voice. Its delay is 2.7 ms longer than in 0.4.3.
- Mic and speaker: new `gpu_keep_awake` setting (Service page). Off saves some GPU power, but an app reopening the mic or speaker may get a short gap.

## 0.4.3 - 2026-10-10

Lower and steady mic and speaker delay, and correct webcam colors.

- Mic and speaker: about 5 ms less delay with noise or echo removal, and with no effects.
- Mic and speaker: the delay stays the same for the whole call, also with the camera off. It used to creep up, with short gaps. The GPU now stays at a middle power state while an app has the mic or speaker open, which uses a little more power.
- Camera: YUYV and NV12 webcams use the colors their driver reports, instead of always BT.601 limited range.
- Camera: new `input_color` setting (Camera > Advanced) for YUYV and NV12 webcams with wrong colors.
- Camera: the log warns when the webcam gives a lower frame rate than asked, as many do with YUYV at 1080p.
- Settings window: Webcam format only lists the formats your webcam offers at the chosen size.

## 0.4.2 - 2026-10-09

Lower mic and speaker delay.

- Setup: removes the previous NVIDIA Broadcast build's files after installing a newer one.
- Mic and speaker: a reworked audio pipeline lowers delay.
- Mic and speaker: a slow model frame now causes a short gap instead of extra delay for the rest of the call.

## 0.4.1 - 2026-10-08

One install command for every distribution, plus `.deb` and `.rpm` packages.

- Install: one command for every distribution: `curl -fsSL https://github.com/kengzzzz/broadcast-linux/releases/latest/download/install.sh | bash`. It uses the AUR on Arch, the `.deb` or `.rpm` where they fit, and the tarball elsewhere.
- Install: adds you to the `video` group. On Arch and Debian-based systems, it also installs the camera module and kernel headers when the module is missing.
- Install: rerunning it upgrades and restarts a running service. It replaces an earlier tarball install and keeps your settings and NVIDIA's files.
- Install: downloads are checked against the release's new `SHA256SUMS`.
- Packages: `.deb` for Debian 13+, Ubuntu 26.04+ and Linux Mint based on it, and `.rpm` for Fedora. Both use the distribution's Wine.
- Tarball: needs glibc 2.35+ and Wine 10+ (was glibc 2.39 and Wine 11), so it runs on Ubuntu 22.04, Debian 12 and newer. Distributions with an older Wine need WineHQ stable.
- `pactl` is no longer needed: device lists, the default device and `doctor` ask PipeWire directly. `pipewire-pulse` is now optional, and `doctor` warns when it isn't running.
- New `broadcast-linux devices`: lists microphones and outputs with the node names the config uses.
- Camera: effect frames go straight into the virtual camera, one copy fewer per frame. Needs v4l2loopback 0.14+ with `max_buffers=3` or more. The module config now sets it, and it applies after a reboot or module reload. Otherwise frames are copied as before.

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
