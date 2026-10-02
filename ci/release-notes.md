Download `broadcast-linux-*-x86_64.tar.gz`, unpack it and run `./install.sh`.
The same tarball works on Arch and other x86_64 distributions with glibc 2.39+.

Built on Debian 13 against WineHQ's stable Wine 11.0; exact build versions are in
`BUILD-INFO`. Use Wine 11.x. The Debian-built workers and relay were also tested on
Arch with Wine 11.18 (audio denoising and camera background replacement).

Requires an NVIDIA RTX GPU with the proprietary driver, PipeWire with WirePlumber,
ffmpeg, pactl, and v4l2loopback for the camera. Wine is installed separately;
NVIDIA's files are downloaded by `broadcast-linux setup`.

For an Arch-managed installation, build `packaging/PKGBUILD` with `makepkg -si`.
