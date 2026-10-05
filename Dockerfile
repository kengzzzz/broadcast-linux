# Builds the portable release tarball:
#   docker build --output type=local,dest=dist .
# Debian 13 for glibc 2.39+; the relay and workers build against WineHQ stable Wine 11.
FROM debian:13 AS build
ARG WINE_VERSION=11.0.0.0~trixie-1
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
 && mkdir -p /etc/apt/keyrings \
 && curl -fsSL https://dl.winehq.org/wine-builds/winehq.key | gpg --dearmor -o /etc/apt/keyrings/winehq-archive.key \
 && curl -fsSL -o /etc/apt/sources.list.d/winehq-trixie.sources \
      https://dl.winehq.org/wine-builds/debian/dists/trixie/winehq-trixie.sources \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      "wine-stable=$WINE_VERSION" "wine-stable-amd64=$WINE_VERSION" "wine-stable-i386=$WINE_VERSION" \
      "wine-stable-dev=$WINE_VERSION" gcc g++ libc6-dev clang libclang-dev pkg-config \
      libpipewire-0.3-dev libspa-0.2-dev meson ninja-build git patch \
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
