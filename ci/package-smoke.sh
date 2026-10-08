#!/usr/bin/env bash
# Installs the .deb or .rpm from DIST without optional dependencies and checks that
# everything it ships can load. Runs inside a distro container; there is no GPU here.
set -euo pipefail
dist=$1
if command -v apt-get >/dev/null; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$dist"/*.deb
else
    dnf install -y -q --setopt=install_weak_deps=False "$dist"/*.rpm
fi
broadcast-linux --version
wine --version
# What `broadcast-linux setup` runs first; a missing Wine part can make it hang.
if ! WINEPREFIX=/tmp/prefix WINEDEBUG=-all WINEDLLOVERRIDES='mscoree,mshtml=' \
    timeout 300 wineboot -u; then
    echo "::error::wineboot could not create a prefix"
    exit 1
fi
for bin in /usr/bin/broadcast-linux /usr/bin/broadcast-linux-gui /usr/lib/broadcast-linux/*/*.so \
    /usr/lib/broadcast-linux/wine/x86_64-unix/*.so; do
    if ldd "$bin" | grep 'not found'; then
        echo "::error::$bin has missing libraries"
        exit 1
    fi
done
# Checks fail without a GPU or session; a crash would exit with a signal instead.
status=0
broadcast-linux doctor || status=$?
if (( status > 1 )); then
    echo "::error::doctor exited with $status"
    exit 1
fi
