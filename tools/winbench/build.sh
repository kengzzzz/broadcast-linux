#!/usr/bin/env bash
# Builds target/x86_64-pc-windows-gnu/release/winbench.exe in Docker and lints it.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
docker build -q -t winbench-build . >/dev/null
docker run --rm -u "$(id -u):$(id -g)" -e CARGO_HOME=/src/target/cargo-home -v "$PWD:/src" \
    winbench-build sh -c '
    cargo fmt --check &&
    cargo clippy --release --target x86_64-pc-windows-gnu -- -D warnings -W clippy::pedantic &&
    cargo build --release --target x86_64-pc-windows-gnu'
