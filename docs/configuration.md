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

```sh
systemctl --user restart broadcast-linux
```

## Microphone

Select your real microphone on the **Microphone** page and click **Apply**. To make
the virtual mic the system default, use **Make it the system default** afterwards.

From the terminal, list your sources, set `[mic] input` to the real mic's node name
in the config and reload before changing the default:

```sh
pactl list short sources
# After setting [mic] input in the config:
systemctl --user reload broadcast-linux
pactl set-default-source broadcast_linux_mic
```

Use **NVIDIA Broadcast Mic** as the input in your call or recording app. Studio
Voice takes precedence over noise and room echo removal. Combined noise and echo
removal uses the higher of their strengths.

## Speaker

Enable processing on the **Speaker** page and choose your real output. In the
config, set `[speaker] enabled = true` and `output` to `"default"` or a real sink's
node name from `pactl list short sinks`, then restart the service.

Select **NVIDIA Broadcast Speaker** as your call app's output. Processing is mono
and tuned for speech. Keep music and games on your real output.

## Camera

Choose the real webcam and capture size on the **Camera** page, then apply changes
before starting its preview. The default input is `/dev/video0`. The virtual
output is `/dev/video10`, labelled **Broadcast Camera**.

Use one background effect at a time: replacement image, blur or removal. Removal
fills the background with black because the virtual camera has no transparency.
Replacement images support `~/` paths and are scaled to fill the frame.

With all camera effects off, the webcam passes through. With all mic effects off,
the microphone passes through.

See [CLI usage](cli.md) for camera module setup, service commands and diagnostics.
