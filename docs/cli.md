# CLI usage

The terminal commands provide an alternative to the settings window. NVIDIA
effects still need a Wayland or X11 desktop session.

## Setup

After [installing broadcast-linux](../README.md#install), run as your normal user:

```sh
broadcast-linux setup
```

Setup detects the GPU, downloads the matching NVIDIA Broadcast runtime and
models, checks pinned checksums, asks you to accept NVIDIA's licence and prepares
the Wine prefix. Allow about 8 GB of free space during setup. The NVIDIA download
is about 2.2–2.4 GB depending on your GPU.

`--keep-installer` retains the downloaded NVIDIA installer. `--accept-eula`
accepts NVIDIA's licence without an interactive prompt. Read the licence before
using it. Run `broadcast-linux setup --help` for the available options.

## Prepare the virtual camera

Install your distribution's v4l2loopback package if your kernel does not provide
the module. DKMS packages also need matching kernel headers.

For the AUR package, the module configuration is already installed. Load it and
grant your user access:

```sh
sudo modprobe v4l2loopback
sudo usermod -aG video "$USER"
```

For a tarball installation with the default `~/.local` prefix, first install the
provided module configuration, then run the commands above:

```sh
sudo install -Dm644 "$HOME/.local/share/broadcast-linux/modules-load.conf" /etc/modules-load.d/broadcast-linux.conf
sudo install -Dm644 "$HOME/.local/share/broadcast-linux/modprobe.conf" /etc/modprobe.d/broadcast-linux.conf
```

Use your installation prefix in place of `~/.local` if you changed it. Log out and
back in after joining the `video` group. If v4l2loopback was already loaded with
different options, `broadcast-linux doctor` prints the required steps.

For microphone-only use, set `[camera] enabled = false` in the config and skip
camera setup. See [configuration](configuration.md) for creating and editing the file.

## Start and use

```sh
systemctl --user enable --now broadcast-linux
broadcast-linux doctor
```

Choose **NVIDIA Broadcast Mic** and **Broadcast Camera** in your call or recording
app. Mic noise removal is enabled by default. Camera effects are off. To process
incoming call audio, enable the speaker in the config, restart the service and
select **NVIDIA Broadcast Speaker** as the call app's output.

## Manage the service

| Task | Command |
| --- | --- |
| Start | `systemctl --user start broadcast-linux` |
| Stop | `systemctl --user stop broadcast-linux` |
| Reload settings | `systemctl --user reload broadcast-linux` |
| Restart or use an updated binary | `systemctl --user restart broadcast-linux` |
| Disable automatic startup | `systemctl --user disable --now broadcast-linux` |
| Check setup | `broadcast-linux doctor` |
| Read this boot's logs | `journalctl --user -u broadcast-linux -b` |

[Configuration](configuration.md#apply-manual-edits) explains which changes need
a restart. To run in the foreground, stop the user service first, then run
`broadcast-linux run`.

Config defaults to `~/.config/broadcast-linux/config.toml`. Downloaded NVIDIA files
and the Wine prefix live in `~/.local/share/broadcast-linux/`. `XDG_CONFIG_HOME`
and `XDG_DATA_HOME` override these locations.
