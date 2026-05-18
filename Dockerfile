FROM rust:1.80-slim AS builder

WORKDIR /src
ENV RUSTUP_TOOLCHAIN=1.80.0

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --release --bin smfs

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    fuse3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/smfs /usr/local/bin/smfs

ENTRYPOINT ["smfs"]
CMD ["--help"]
