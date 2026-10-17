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
is about 2.2 to 2.4 GB depending on your GPU.

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
| List microphones and outputs | `broadcast-linux devices` |
| Run effects on files | `broadcast-linux process --help` |
| Read this boot's logs | `journalctl --user -u broadcast-linux -b` |

[Configuration](configuration.md#apply-manual-edits) explains which changes need
a restart. To run in the foreground, stop the user service first, then run
`broadcast-linux run`.

## Process files

`broadcast-linux process` runs the effects on video and audio files, like the **Files**
page. It needs `ffmpeg`.

```sh
broadcast-linux process --noise-removal --blur talk.mp4
broadcast-linux process --studio-voice=warm -o ~/Music/clean *.wav
broadcast-linux process --super-resolution 2160 --frame-generation=2 clip.mov
```

Optional values go after `=`, as in `--blur=0.75`. `broadcast-linux process --help`
lists every effect.

- New files go beside the original, or in the `-o` folder, named `NAME-broadcast`.
  Existing files are never replaced.
- Video effects make H.264 MP4 with AAC audio (libx264, else NVENC, else OpenH264).
- Audio effects alone copy the video. MP4 and MOV keep their format, others become MKV.
- WAV and FLAC stay WAV and FLAC, other audio becomes M4A. Audio comes out at 48 kHz,
  with more than two channels mixed to stereo.
- `--super-resolution` takes the new height (1080, 1440 or 2160) and only enlarges.
- `--frame-generation` multiplies the frame rate by 2, 3 or 4.
- Ctrl+C stops and removes the unfinished file.

The live effects keep running but share the GPU, so they may slow down meanwhile.

Config defaults to `~/.config/broadcast-linux/config.toml`. Downloaded NVIDIA files
and the Wine prefix live in `~/.local/share/broadcast-linux/`. `XDG_CONFIG_HOME`
and `XDG_DATA_HOME` override these locations.
