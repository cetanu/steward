FROM rust:1.88-bookworm AS builder
RUN apt-get update \
    && apt-get install --no-install-recommends --yes protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /project
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src src
RUN cargo build --release --locked --bin steward

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 steward \
    && useradd --uid 10001 --gid 10001 --create-home --home-dir /home/steward --shell /usr/sbin/nologin steward

WORKDIR /project
COPY --chown=10001:10001 --from=builder /project/target/release/steward /project/steward

USER 10001:10001
EXPOSE 5001

CMD [ "/project/steward" ]
