# broadcast-linux

NVIDIA Broadcast's effects on Linux, as a virtual microphone and a virtual camera
that any app can pick like a normal device:

- **Mic:** noise removal, room echo removal, Studio Voice
- **Camera:** video noise removal, background replacement, Studio Light (relighting)

It runs NVIDIA's own Windows effects runtime and models, the same ones the Windows
NVIDIA Broadcast app uses, under Wine. The effects only run while an app uses a
device, so an idle system has no model loaded and the real mic and camera are closed.

NVIDIA's files are not part of this project. `broadcast-linux setup` downloads the
official NVIDIA Broadcast installer for your GPU from NVIDIA, shows NVIDIA's licence,
and extracts only what the effects need.

## Requirements

- An NVIDIA RTX GPU (Turing or newer) with the proprietary driver
- Wine 11 (the nvcuda relay does not build against Wine 10; releases are built
  against WineHQ's Wine 11.0 and tested on 11.18)
- PipeWire with WirePlumber, `pactl` (libpulse), ffmpeg
- The v4l2loopback kernel module, for the camera

Only the Blackwell (RTX 50) installer has a pinned checksum so far. On other GPU
generations, run `broadcast-linux setup --allow-unverified`, which checks the size only.

## Install

**Arch Linux:** build `packaging/PKGBUILD` (also meant for the AUR), which compiles
everything against your installed Wine and PipeWire:

```sh
cd packaging && makepkg -si
```

**Other distributions:** download `broadcast-linux-<version>-x86_64.tar.gz` from the
[releases](https://github.com/kengzzzz/broadcast-linux/releases) (needs glibc 2.39+, e.g. Ubuntu 24.04 or Debian 13),
unpack it and run `./install.sh`. It installs to `~/.local` (set `PREFIX` to change
that) and prints the few steps that need root. To build from source instead, see
[Building](#building).

Then, as your user:

```sh
broadcast-linux setup
systemctl --user enable --now broadcast-linux
```

For the camera, v4l2loopback must be loaded with the options in
`packaging/modprobe.conf` (the package installs them), and your user must be in the
`video` group: `sudo usermod -aG video $USER`, then log in again.

Apps now list **NVIDIA Broadcast Mic** and **Broadcast Camera**. To make the mic the
system default input, set `[mic] input` to your real mic's node name
(`pactl list short sources`) and run `pactl set-default-source broadcast_linux_mic`.

## Configuration

`~/.config/broadcast-linux/config.toml`; see `packaging/config.toml` for every option.
Apply changes with `systemctl --user reload broadcast-linux`. Changing the camera
resolution or the mic name needs a restart.

```toml
[mic]
input = "alsa_input.usb-..."          # or "default"
noise_removal = { enabled = true, strength = 1.0 }
room_echo_removal = { enabled = true, strength = 1.0 }
studio_voice = { enabled = false }    # replaces noise/echo removal while on
unload_after_minutes = 10

[camera]
input = "/dev/video0"
width = 1920
height = 1080
fps = 30
video_noise_removal = { enabled = true }
background = "~/Pictures/background.jpg"
studio_light = { enabled = true, strength = 1.0, preset = "neutral" }
```

## Remote desktop (FreeRDP)

Point FreeRDP at the virtual devices, and allow only PCM for the microphone. With
compressed formats Windows keeps changing the bitrate, and FreeRDP reopens the mic on
every change, which chops short words:

```sh
sdl-freerdp3 ... /microphone:sys:pulse,format:1 /dvc:rdpecam
```

## Building

With the submodules checked out (`git submodule update --init`), Rust, Wine 11
(`winegcc` and headers), clang, meson and ninja:

```sh
./build.sh build/stage     # build/stage/bin and build/stage/lib/broadcast-linux
docker build --output type=local,dest=dist .   # the release tarball, in Docker
```

`tools/measure/` has the mic regression tests and `tools/dev/` has offline harnesses
for the Wine workers.

## How it works

A small Rust service owns the virtual devices: a PipeWire source for the mic and a
v4l2loopback device for the camera. When an app starts reading, it starts a worker
program under Wine that loads NVIDIA's `NVAudioEffects.dll` or `NVVideoEffects.dll`
and streams audio or frames through it. CUDA reaches the Linux driver through
[SveSop's nvcuda relay](https://github.com/SveSop/nvcuda) plus a small patch
(`patches/`), and the video effects use DXVK and DXVK-NVAPI.

## Licence

MIT (see `LICENSE`). The nvcuda relay and the patch to it are LGPL-2.1-or-later.
NVIDIA Broadcast's files are covered by NVIDIA's licence, which you accept during
`setup`; they are downloaded on your machine and never redistributed.
