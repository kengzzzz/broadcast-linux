# Configuration

Open **broadcast-linux** from your app menu, or run `broadcast-linux-gui`, to select
devices, change effects and preview the camera. Click **Apply** to save changes.
The window reloads or restarts the running service as needed. If the service is
stopped, changes are saved for its next start.

## Config file

Settings live in `~/.config/broadcast-linux/config.toml`, or
`$XDG_CONFIG_HOME/broadcast-linux/config.toml` when `XDG_CONFIG_HOME` is set.
The settings window preserves comments when editing the file.

[The example config](../packaging/config.toml) lists every option, allowed values
and defaults. Omitted settings use their defaults: mic noise removal on, speaker
disabled, camera enabled with effects off.

The tarball installer creates a config if one does not exist. With the AUR package,
copy the installed example before editing it:

```sh
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/broadcast-linux"
mkdir -p "$config_dir"
cp -n /usr/share/doc/broadcast-linux/config.toml "$config_dir/config.toml"
```

## Apply manual edits

Reload the running service after editing the file:

```sh
systemctl --user reload broadcast-linux
```

Restart instead when changing these fields:

- `[mic]` or `[speaker]`: `enabled`, `name`
- `[camera]`: `enabled`, `device`, `width`, `height`
- `[service]`: `gpu`

```sh
systemctl --user restart broadcast-linux
```

Apps that still use the camera at the old size keep it, with a black picture, until they
close the camera. Then it switches to the new size.

## Microphone

Select your real microphone on the **Audio** page and click **Apply**. To make
the virtual mic the system default, use **Make it the system default** afterwards.

From the terminal, list your microphones, set `[mic] input` to the real mic's node
name in the config and reload before changing the default:

```sh
broadcast-linux devices
# After setting [mic] input in the config:
systemctl --user reload broadcast-linux
wpctl set-default ID    # the ID of broadcast_linux_mic in the list above
```

Use **NVIDIA Broadcast Mic** as the input in your call or recording app. Studio
Voice takes precedence over noise and room echo removal. Combined noise and echo
removal uses the higher of their strengths.

Studio Voice can use a mic profile: Bright, Full or Warm. Pick it under **Studio Voice**
or set `studio_voice = { enabled = true, profile = "warm" }`. Profiles need NVIDIA
Broadcast 2.3.0 and add about 30 ms of delay.

## Speaker

Turn on **Virtual speaker** on the **Audio** page and choose your real output. In the
config, set `[speaker] enabled = true` and `output` to `"default"` or a real sink's
node name from `broadcast-linux devices`, then restart the service.

Select **NVIDIA Broadcast Speaker** as your call app's output. Processing is mono
and tuned for speech. Keep music and games on your real output.

## Camera

Choose the real webcam on the **Video** page, and the capture size in the menu under
the preview. Apply changes before turning on the preview. The default input is `/dev/video0`. The virtual
output is `/dev/video10`, labelled **Broadcast Camera**.

Use one background effect at a time: replacement image, blur or removal. Removal
fills the background with black because the virtual camera has no transparency.
Replacement images support `~/` paths and are scaled to fill the frame.

With all camera effects off, the webcam passes through. With all mic effects off,
the microphone passes through.

**Super Resolution** captures the webcam smaller, for example 1280 × 720 for
1920 × 1080, and upscales it with AI. Pick the output size in the menu under the
preview. **Quality** looks best, **Performance** uses less GPU. It needs an RTX 30 series
or newer and the driver's `_nvngx.dll`, in `nvidia/wine` beside `libGLX_nvidia.so.0`.

**Frame Generation** captures at the frame rate divided by the multiplier and makes the
frames in between with AI, so 60 fps at 2× captures 30 fps. The frame rate must divide
by the multiplier. Each made frame adds one output frame of delay, about 17 ms at 60 fps.
It needs an RTX 40 series or newer.

Both run after the other effects. `broadcast-linux doctor` (`rtx video`) says whether
they can run.

## GPU load

A bar at the bottom of the window shows the effects' time per frame against the time
between frames, for example 27 of 33 ms (81%). Over 100%, the camera drops frames or the
audio falls behind. The camera's frame rate is shown against the one set. It is lower when
the webcam can't deliver it. "Whole GPU" is how busy the GPU was, including games and the
desktop. Light loads run at a lower clock, so they read higher.

## Several GPUs

With more than one NVIDIA GPU, **Settings** has a **Run effects on** list. Automatic
uses the first supported GPU in `nvidia-smi -L`. In the config, `[service] gpu` takes a
UUID from that list. All effects run on the chosen GPU.

Setup installs NVIDIA's files for one GPU generation. After picking a GPU of another
generation, click **Apply**, then **Download and install**. If the configured GPU is
missing, the effects use the automatic one and `doctor` warns.

## File Processor

The **Files** page runs the effects on video and audio files, like NVIDIA's File
Processor on Windows. Add files, turn on effects, pick the output folder and click
**Process**. Its settings aren't saved. Dropping files on the window only works on X11.

It needs `ffmpeg`. [CLI usage](cli.md#process-files) has the command and output formats.

See [CLI usage](cli.md) for camera module setup, service commands and diagnostics.
