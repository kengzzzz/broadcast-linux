# broadcast-linux

NVIDIA Broadcast effects on Linux, with a native settings window and virtual mic,
speaker and camera. Effects run through Wine and start when an app uses them.

![Settings window: microphone and camera effects, then the setup checks](docs/demo.gif)

- **Microphone:** noise removal, room echo removal, Studio Voice
- **Speaker:** noise and room echo removal for incoming call audio
- **Camera:** video noise removal, background replacement/blur/removal, Studio Light,
  Eye Contact, Auto Frame

## Requirements

### NVIDIA hardware

[NVIDIA's published hardware requirements](https://www.nvidia.com/en-us/geforce/broadcasting/broadcast-app/faq/):

- GeForce RTX 2060, Quadro RTX 3000, TITAN RTX or newer
- 8 GB RAM or more
- Recommended CPU: Intel Core i5-8600 or AMD Ryzen 5 2600 or newer
- Studio Voice and Studio Light (Virtual Key Light) require a GeForce RTX 3060 desktop GPU
  or higher

Only tested on an RTX 50 card. NVIDIA ships a separate installer for each GPU generation,
RTX 20 to RTX 50. Their 43 program DLLs are byte-identical apart from the signature, and
only the AI model files are built per generation, so RTX 20, 30 and 40 cards should work
too. Reports are welcome in [issue #1](https://github.com/kengzzzz/broadcast-linux/issues/1).

### Linux app

- Packaged: Arch Linux, Fedora, Debian 13+, Ubuntu 26.04+, and Ubuntu 26.04-based Linux Mint
- Generic tarball: other x86_64 distributions with systemd and glibc >= 2.35
- NVIDIA proprietary driver [>= 575.51.02](https://github.com/doitsujin/dxvk/wiki/Driver-support)
- Wine >= 10
- PipeWire with WirePlumber, plus `pipewire-pulse` for apps that use PulseAudio
- Wayland or X11 desktop session with OpenGL
- 8 GB free disk space and internet access during setup
- The virtual camera requires v4l2loopback >= 0.12.6 and your user in the `video` group

## Install

On any supported distribution, as your normal user:

```sh
curl -fsSL https://github.com/kengzzzz/broadcast-linux/releases/latest/download/install.sh | bash
```

It uses the AUR package on Arch, the `.deb` on Debian, Ubuntu and Mint, the `.rpm` on
Fedora, and the tarball elsewhere. It also sets up the virtual camera and asks for `sudo`
when needed. Run it again to upgrade. Your settings are kept.

To install by hand instead:

**Arch Linux (AUR):**

```sh
paru -S broadcast-linux-bin
```

For the virtual camera, also install `v4l2loopback-dkms` and your kernel's headers
(`linux-headers` for the default kernel), unless your kernel already has the module.

**Debian, Ubuntu and Linux Mint:** download the `.deb` from the
[latest release](https://github.com/kengzzzz/broadcast-linux/releases/latest), then:

```sh
sudo apt install ./broadcast-linux_*_amd64.deb
```

Debian doesn't install kernel headers by default, and the camera module needs them to build.
Install `linux-headers-amd64` too. Ubuntu usually has them already.

**Fedora:** enable [RPM Fusion](https://rpmfusion.org/Configuration) first, so the virtual
camera's module (`akmod-v4l2loopback`) installs with the package. Download the `.rpm` from
the [latest release](https://github.com/kengzzzz/broadcast-linux/releases/latest), then:

```sh
sudo dnf install ./broadcast-linux-*.x86_64.rpm
```

**Other distributions:** install the requirements above, download and unpack the
[release tarball](https://github.com/kengzzzz/broadcast-linux/releases), then run
`./install.sh` inside it. It installs to `~/.local`. Add `~/.local/bin` to your `PATH`.

For the virtual camera, add yourself to the `video` group, then log out and back in:

```sh
sudo usermod -aG video "$USER"
```

## Get started

1. Open **broadcast-linux** from your app menu, or run `broadcast-linux-gui`.
2. On **Setup**, click **Download and install** and accept NVIDIA's licence.
   The download is about 2.2–2.4 GB depending on your GPU. Start the service on the same page.
3. Choose your devices and effects, then **Apply**. For the camera, follow the
   **Camera** page's module and permission steps. Log in again after joining the video group.
4. In your call or recording app, select **NVIDIA Broadcast Mic** and **Broadcast Camera**.
   Enable the speaker if needed and select **NVIDIA Broadcast Speaker** as the app's output.

Mic noise removal is on by default. Speaker processing and camera effects are off.
The **Setup** page includes diagnostics and service logs.

## Documentation

- [Configuration](docs/configuration.md): manual settings, device routing and applying changes
- [CLI usage](docs/cli.md): setup and service management from the terminal
- [Building](docs/building.md): source builds, release tarballs and maintainer steps
- [Architecture](docs/architecture.md): how the service, Wine workers and pipelines fit together
- [Benchmark](docs/benchmark.md): delay, CPU and GPU power, compared with NVIDIA Broadcast on Windows

## Licence

[MIT](LICENSE). Bundled [nvcuda](https://github.com/SveSop/nvcuda) relay: LGPL-2.1-or-later.
Statically linked libjpeg-turbo: [IJG and BSD-3-Clause](packaging/LICENSE.libjpeg-turbo).
The settings window includes [fonts under their own licences](packaging/LICENSE.fonts).
NVIDIA files are downloaded under NVIDIA's licence, accepted during setup.
