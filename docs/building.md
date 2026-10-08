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

The output is `build/stage`, containing the CLI, settings window, patched nvcuda
relay and Wine workers. You can supply a different output directory as the first
argument to `build.sh`. This stages binaries. Use a package or release tarball for
the desktop entry, config and systemd unit.

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

For a release, align the workspace version in `Cargo.toml`, `Cargo.lock` and both
`PKGBUILD` files with the `v<version>` tag. Rename `## Unreleased` in
[CHANGELOG.md](../CHANGELOG.md) to `## <version> - <date>`. That section becomes
the release notes. Pushing a `v*` tag builds the tarball, the `.deb` and the `.rpm`,
installs the packages on Debian 13, Ubuntu 26.04 and Fedora 44, then publishes all three
with `SHA256SUMS` and the top-level `install.sh`, which picks the right one for the user's distribution.

See [AUR publishing](../packaging/README.md) for publishing the binary package.

## Updating NVIDIA Broadcast

The `nvidia-update` workflow runs every Monday and can be started by hand. When
NVIDIA's Broadcast page offers a newer build, it opens an issue with the new installer
entries and checks that each installer still has the files the code names. It also
fails when a pinned NVIDIA installer is gone or has changed size, because setup then
breaks for new users. Run the same check locally with `ci/nvidia-update.sh report.md`. It needs
7-Zip and downloads about 9 GB when a new build exists.

To move to a new build:

1. Paste the issue's `BUILD` and installer entries into `src/setup.rs`.
2. If the issue lists missing files, update the model folder and file names in
   `src/nvidia.rs` and `src/config.rs`, or the paths `extract` keeps in `src/setup.rs`.
3. Run setup, then test every microphone, speaker and camera effect on a GPU. A newer
   CUDA runtime in NVIDIA's files can ask for a driver export table that the bundled
   nvcuda relay lacks, like the one `patches/nvcuda-export-table-d2688bf2.patch` adds.
   That only shows up when an effect starts.
4. Add a CHANGELOG entry. Users rerun setup after upgrading, and setup then removes
   the previous build's files.
