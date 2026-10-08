# Builds the portable release tarball:
#   docker build --output type=local,dest=dist .
# Debian 12 keeps the glibc floor at 2.35. The relay needs Wine 11 headers and meson 1.3+
# (backports); the result runs on Wine 10+.
FROM debian:12 AS build
ARG WINE_VERSION=11.0.0.0~bookworm-1
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
 && mkdir -p /etc/apt/keyrings \
 && curl -fsSL https://dl.winehq.org/wine-builds/winehq.key | gpg --dearmor -o /etc/apt/keyrings/winehq-archive.key \
 && curl -fsSL -o /etc/apt/sources.list.d/winehq-bookworm.sources \
      https://dl.winehq.org/wine-builds/debian/dists/bookworm/winehq-bookworm.sources \
 && echo 'deb http://deb.debian.org/debian bookworm-backports main' > /etc/apt/sources.list.d/backports.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends -t bookworm-backports meson \
 && apt-get install -y --no-install-recommends \
      "wine-stable=$WINE_VERSION" "wine-stable-amd64=$WINE_VERSION" "wine-stable-i386=$WINE_VERSION" \
      "wine-stable-dev=$WINE_VERSION" gcc g++ libc6-dev clang libclang-dev pkg-config \
      libpipewire-0.3-dev libspa-0.2-dev ninja-build git patch cmake make nasm \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/root/.cargo/bin:/opt/wine-stable/bin:$PATH
WORKDIR /src
COPY rust-toolchain.toml ./
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
 && rustc --version
COPY . .
RUN CARGO_ARGS=--locked ci/release-tarball.sh /dist

FROM scratch
COPY --from=build /dist/ /
