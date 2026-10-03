# Container Definitions & Testbed Harnesses

This directory contains container build specifications for both release packaging and repository-internal integration testing.

---

## 1. Production Release Artifacts

The following files define standalone production container images:

* **[`Dockerfile`](Dockerfile)**:
  Multi-stage production build definition:
  - **Stage 1 (Builder):** Uses `rust:1.88-bookworm` to compile the release binary (`cargo build --release --locked --bin steward`).
  - **Stage 2 (Runtime):** Uses minimal `debian:bookworm-slim` with updated CA certificates.
  - **Security:** Runs as an unprivileged, non-root user (`steward: 10001:10001`).
  - **Independence:** Contains strictly the compiled `/project/steward` binary and OS certificates. It contains **no** baked-in policy files, mock hostnames, or test configurations.
  - **Operator Configuration:** Supply configuration via standard container volume mounts (e.g. at `/etc/steward/steward.yaml`) or via 12-factor environment variables (`STEWARD__*`, `REDIS_URL`).

* **[`Dockerfile.prebuilt`](Dockerfile.prebuilt)**:
  Packaging image used in CI to wrap pre-compiled CI runner binaries (`target/release/steward`) for Trivy vulnerability scanning and fast integration testing.

---

## 2. Local Integration Testbed Harnesses (Not Released)

The remaining files in this directory and `docker-compose.yml` are strictly for **local development and CI integration tests** within this repository:

* **[`envoy.Dockerfile`](envoy.Dockerfile) & [`envoy.yaml`](envoy.yaml)**:
  Configures an Envoy proxy (v1.39) as an RLS client to exercise circuit breaking, gRPC timeouts (20 ms), HTTP/2 load balancing across replicas, and rate-limit headers (`x-ratelimit-*`).
* **[`mock_config.Dockerfile`](mock_config.Dockerfile) & [`mock_server.py`](mock_server.py)**:
  A lightweight Python HTTP server used in integration tests to serve dynamic policy JSON configurations and verify HTTP conditional fetch (ETag / 304 Not Modified).
* **[`steward.yaml`](steward.yaml)**:
  Docker Compose testbed configuration pointing Steward to `http://mock_config:8000/api/rate_limits` and `redis_host: redis`. In `docker-compose.yml`, this file is mounted at `/project/steward.yaml:ro` for development containers.
