# Verification

Use these checks when changing Steward. They exercise the checked-out source; they are not a certification of a particular release artifact or deployment.

Pull requests run the regular CI checks and Envoy integration test. The benchmark, canary, and chaos harnesses run on pushes to `main` and when a release tag is explicitly selected for dispatch; release jobs wait for them to pass. Their logs are retained as a workflow artifact.

## Tests

Run the Rust unit tests:

```bash
cargo test --locked --lib
```

Run the Envoy integration test with Docker Compose:

```bash
make test
```

The integration target starts the Compose environment before running `tests/rate_limit.rs`.

## Performance

Run the [benchmark harness](benchmarks.md) to measure local throughput and latency. Record the machine, Redis version and settings, network placement, and benchmark output with any published result. The measurements are environment-specific and should not be used as a capacity promise for another deployment.

## Standalone harnesses

`cargo test` does not run the benchmark, canary, or chaos harness scenarios. CI invokes these executable programs explicitly on `main` and for release-tag dispatches:

```bash
cargo run --release --bin canary_harness
cargo run --release --bin chaos_harness
```

Each starts local test services and Redis processes. The chaos harness deliberately pauses and terminates its test Redis process, so run it in an isolated development environment. The benchmark command and its port requirements are documented in [Benchmarking](benchmarks.md).
