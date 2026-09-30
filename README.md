# broadcast-linux

NVIDIA Broadcast's effects on Linux, as a virtual mic and camera any app can use.

- **Mic:** noise removal, room echo removal, Studio Voice
- **Camera:** video noise removal, background replacement, Studio Light

It runs NVIDIA's own Windows effects runtime under Wine, only while an app uses the
device. NVIDIA's files are not included: `broadcast-linux setup` downloads the
official NVIDIA Broadcast installer for your GPU.

## Requirements

- NVIDIA RTX GPU (Turing or newer) with the proprietary driver
- Wine 11
- PipeWire with WirePlumber, `pactl`, ffmpeg
- v4l2loopback (for the camera)

## Install

**Arch Linux:** `cd packaging && makepkg -si`

**Other distributions:** download the tarball from
[releases](https://github.com/kengzzzz/broadcast-linux/releases) (glibc 2.39+),
unpack it and run `./install.sh`.

Then:

```sh
broadcast-linux setup
systemctl --user enable --now broadcast-linux
sudo usermod -aG video $USER    # for the camera; log in again afterwards
```

Only the RTX 50 installer has a pinned checksum; on older GPUs use
`broadcast-linux setup --allow-unverified`.

## Configuration

`~/.config/broadcast-linux/config.toml`; every option is described in
[`packaging/config.toml`](packaging/config.toml). Apply changes with
`systemctl --user reload broadcast-linux`.

To make the virtual mic the default input, set `[mic] input` to your real mic
(`pactl list short sources`), then run `pactl set-default-source broadcast_linux_mic`.

## Building

```sh
git submodule update --init
./build.sh                                      # into build/stage
docker build --output type=local,dest=dist .    # release tarball
```

Needs Rust, Wine 11 (`winegcc` and headers), clang, meson and ninja.

## Licence

MIT. The bundled nvcuda relay ([SveSop/nvcuda](https://github.com/SveSop/nvcuda)
plus `patches/`) is LGPL-2.1-or-later. NVIDIA's files are covered by NVIDIA's
licence, accepted during `setup`, and are never redistributed.
