# Building

## Local build

Install Rust through rustup, Wine 11+ with `winegcc` and its headers, PipeWire/SPA
development headers, a C/C++ toolchain, clang/libclang, pkg-config, meson, ninja,
cmake, nasm, git and patch. [rust-toolchain.toml](../rust-toolchain.toml) pins the
Rust version used locally and in CI.

From the repository root:

```sh
git submodule update --init
./build.sh
```

The output is `build/stage` (or `build.sh`'s first argument): the CLI, settings window,
patched nvcuda relay and Wine workers. For the desktop entry, config and systemd unit,
use a package or the release tarball.

On Arch Linux, build and install the source package instead:

```sh
cd packaging
makepkg -si
```

The source package builds the tag selected by `pkgver` in
[PKGBUILD](../packaging/PKGBUILD).

## Release tarball

With Docker installed, from the repository root:

```sh
git submodule update --init
docker build --output type=local,dest=dist .
```

The tarball in `dist/` includes `install.sh` and requires glibc 2.35+ and Wine 10+ on
the target system. See [installation](../README.md#install) and [CLI setup](cli.md).

To repackage it as a `.deb` and an `.rpm` in `dist/` (uses Docker for
[nfpm](https://nfpm.goreleaser.com)):

```sh
ci/release-packages.sh dist
```

The package contents and dependencies are in [nfpm.yaml](../packaging/nfpm.yaml).

## Checks and releases

CI runs formatting, Clippy and workspace tests on code pushes to `main` and pull
requests. Run the same checks locally:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings -W clippy::pedantic
cargo test --locked --workspace
```

For a release:

1. Set the version in `Cargo.toml`, `Cargo.lock` and both `PKGBUILD` files.
2. Rename `## Unreleased` in [CHANGELOG.md](../CHANGELOG.md) to `## <version> - <date>`.
   That section becomes the release notes.
3. Push a `v<version>` tag. CI builds the tarball, `.deb` and `.rpm`, test-installs the
   packages on Debian 13, Ubuntu 26.04 and Fedora 44, and publishes them with
   `SHA256SUMS` and `install.sh`, which picks the right one for each distribution.
4. Publish the AUR binary package: see [AUR publishing](../packaging/README.md).

## Updating NVIDIA Broadcast

The `nvidia-update` workflow runs every Monday, or by hand:

- A newer build on NVIDIA's Broadcast page opens an issue with the new installer
  entries, and checks each installer still has the files the code names.
- It fails when a pinned installer is gone or changed size, since setup then breaks for
  new users.
- Locally: `ci/nvidia-update.sh report.md`. Needs 7-Zip, and downloads about 9 GB when a
  new build exists.

To move to a new build:

1. Paste the issue's `BUILD` and installer entries into `src/setup.rs`.
2. If the issue lists missing files, update the model folder and file names in
   `src/nvidia.rs` and `src/config.rs`, or the paths `extract` keeps in `src/setup.rs`.
3. Run setup, then test every microphone, speaker and camera effect on a GPU. A newer
   CUDA runtime can need a driver export table the nvcuda relay lacks, like the one
   `patches/nvcuda-export-table-d2688bf2.patch` adds. It only fails when an effect starts.
4. Add a CHANGELOG entry. Users rerun setup after upgrading, which removes the previous
   build's files.
