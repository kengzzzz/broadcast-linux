# broadcast-linux

NVIDIA Broadcast effects on Linux, exposed as a virtual mic and camera.

- **Mic:** noise removal, room echo removal, Studio Voice
- **Camera:** video noise removal, background replacement/blur/removal, Studio Light

Effects start on demand under Wine. `broadcast-linux setup` downloads NVIDIA's
runtime and models for your GPU.

## Requirements

- x86_64 Linux (release tarball requires glibc 2.39+)
- NVIDIA RTX GPU (Turing or newer) with the proprietary driver
- Wine 11.x, PipeWire and its client library, WirePlumber, `pactl`, ffmpeg
- v4l2loopback and membership in the `video` group for the camera

## Install

**Arch (AUR):** `paru -S broadcast-linux-bin` downloads the release without compiling.

**Tarball:** download and unpack the [release tarball](https://github.com/kengzzzz/broadcast-linux/releases)
(same build for Arch and other distributions), then run `./install.sh`.
It installs to `~/.local`; ensure `~/.local/bin` is in your `PATH`.

For the camera, complete the package or installer's printed module and group setup
steps before starting the service, then log in again. Run as your normal user:

```sh
broadcast-linux setup
systemctl --user enable --now broadcast-linux
```

Only the RTX 50 installer has a pinned checksum; older GPUs require
`broadcast-linux setup --allow-unverified`.

In your app, select **NVIDIA Broadcast Mic** and **Broadcast Camera**.
Mic noise removal is enabled by default; camera effects are off.
For errors, run `journalctl --user -u broadcast-linux -b`.

To build the package from source, run `cd packaging && makepkg -si`.

## Configuration

The AUR package includes `/usr/share/doc/broadcast-linux/config.toml` and this guide.
To create an editable config without replacing an existing one:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/broadcast-linux"
cp -n /usr/share/doc/broadcast-linux/config.toml "${XDG_CONFIG_HOME:-$HOME/.config}/broadcast-linux/config.toml"
```

Edit the copied config; [all options](https://github.com/kengzzzz/broadcast-linux/blob/main/packaging/config.toml)
include allowed values and defaults. Apply with `systemctl --user reload broadcast-linux`.
Restart instead for mic/camera `enabled`, mic `name`, or camera `device`, `width`, `height`.
Choose one background effect: image, blur or removal. Removal fills the background black.

To use Broadcast Mic as the default, set `[mic] input` to your real mic's node name
(`pactl list short sources`), then run `pactl set-default-source broadcast_linux_mic`.

## Building

```sh
git submodule update --init
./build.sh                                    # build/stage
docker build --output type=local,dest=dist .   # release tarball
```

Local build: Rust, Wine 11 (`winegcc` and headers), PipeWire headers, clang,
pkg-config, meson, ninja.

CI checks code pushes to `main` and pull requests; Markdown/licence-only changes
skip checks. `v*` tags publish one tarball after checks pass. Keep `Cargo.toml`,
`Cargo.lock` and both `PKGBUILD` versions aligned with the tag.
AUR publishing steps: [packaging/README.md](packaging/README.md).

## Licence

MIT. Bundled [nvcuda](https://github.com/SveSop/nvcuda) relay: LGPL-2.1-or-later.
NVIDIA files are downloaded under NVIDIA's licence, accepted during `setup`.
