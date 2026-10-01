FROM rust:1.88-bookworm
RUN apt-get update \
    && apt-get install --no-install-recommends --yes protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt

WORKDIR /project

# Build dependencies
RUN mkdir src
RUN echo "fn main() {}" >> src/main.rs
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY rust-toolchain.toml rust-toolchain.toml
RUN cargo build --locked
RUN rm -f src/main.rs

# Build protobufs + binary
COPY build.rs build.rs
COPY src src
RUN cargo build --locked
COPY containers/steward.yaml steward.yaml

CMD [ "/project/target/debug/steward" ]
