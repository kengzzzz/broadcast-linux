## Install

- **Arch:** `paru -S broadcast-linux-bin`
- **Other x86_64 distributions (glibc 2.39+):** download
  `broadcast-linux-*-x86_64.tar.gz`, unpack it and run `./install.sh`.

Then run `broadcast-linux setup` and see the
[README](https://github.com/kengzzzz/broadcast-linux#install) for the camera and
service steps. Requires an NVIDIA RTX GPU with the proprietary driver, Wine 11.x,
PipeWire with WirePlumber and pactl, plus v4l2loopback for the camera.
Build versions are in `BUILD-INFO`.
