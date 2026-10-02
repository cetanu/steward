FROM rust:1.88-bookworm AS builder
RUN apt-get update \
    && apt-get install --no-install-recommends --yes protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /project
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /project
COPY containers/steward.yaml steward.yaml
COPY --from=builder /project/target/release/steward /project/steward

CMD [ "/project/steward" ]
