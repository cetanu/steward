# Benchmarking

The repository includes an open-loop gRPC benchmark harness. It runs the Steward service against a temporary local Redis instance and prints throughput, latency percentiles, and memory use for several algorithms and request shapes.

Run it with:

```bash
cargo run --release --bin bench_harness
```

The harness requires `redis-server` and Linux `/proc` for memory measurements. It uses local ports `16379` for Redis and `50051` for gRPC; make sure those ports are free. The temporary Redis process is stopped when the harness exits.

## Interpreting results

Treat output as a measurement of the machine and setup where the harness ran, not as a service capacity guarantee. Results depend on CPU, Redis version and settings, network path, policy shape, key distribution, and offered load. Measure with a workload and deployment topology that resemble yours before setting capacity or latency targets.

The repository previously contained conflicting benchmark figures from different qualification runs. Those figures have been removed rather than presented as a release guarantee. Save new output with the environment details whenever you publish or compare results.
