FROM rust:1
RUN rustup component add rustfmt

WORKDIR /project

# Build dependencies
RUN mkdir src
RUN echo "fn main() {}" >> src/main.rs
COPY Cargo.toml Cargo.toml
RUN cargo build
RUN rm -f src/main.rs

# Build protobufs + binary
COPY build.rs build.rs
COPY src src
RUN cargo build
COPY containers/steward.yaml steward.yaml

CMD [ "/project/target/debug/steward" ]
