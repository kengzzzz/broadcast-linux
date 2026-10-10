FROM debian:12
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev gcc-mingw-w64-x86-64 \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | CARGO_HOME=/opt/cargo sh -s -- -y --no-modify-path \
      --profile minimal --default-toolchain 1.99.0 --component clippy,rustfmt \
      --target x86_64-pc-windows-gnu \
 && chmod -R a+rX /opt/rustup /opt/cargo
WORKDIR /src
