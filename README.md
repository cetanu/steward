steward
============================================================

Mission Statement
------------------------------------------------------------

Steward is an implementation of the Lyft Rate-Limit service.


Features
------------------------------------------------------------

* Load rate limit configs from HTTP or a local file
* Redis-backed fixed-window, token-bucket, and sliding-window rate limiting
* Optional StatsD metrics over UDP


Building and testing
------------------------------------------------------------

### Prerequisites

* Docker
* Docker-compose
* Make

### Optional prerequisites for building locally

* Rust toolchain 1.88 or newer


### Building locally

Simply execute `cargo build --release` to create a binary
which, when run, will start the rate limit service as a
gRPC server.

### Running the environment

The environment can be brought up with `make run`.  
It includes an envoy proxy, the rate limit service, a redis
database, a mock configuration server, and a httpbin backend.

### Running tests

The project uses tavern HTTP integration tests.  
They can be executed with `make test`. Rust unit tests run with
`cargo test`.


Configuration
------------------------------------------------------------

The path to local configuration can be specified using the
environment variable `STEWARD_CONFIG_PATH`.  
The default location is `steward.yaml` in the current working
directory.

Example configuration file:

```yaml
listen:
  addr: 0.0.0.0
  port: 5001
rate_limit_configs:
  http: http://mock_config:8000/api/rate_limits
redis_host: redis
redis_connections: 8
default_ttl: 10
config_refresh_interval_secs: 60
```

### `rate_limit_configs`

This parameter allows specifying a location for the service
to lookup various rate limit configurations.

Either a `Http` or `File` location can be specified.

Example of what the service expects the location to contain:

```json
{
    "domain": [
        {
            "key": "descriptor_key",
            "value": "descriptor_value",
            "rate_limit": {
                "unit": "<seconds|minutes|hours|days|months|years>",
                "requests_per_unit": 12345
            }
        }
    ]
}
```

There can be any number of domains and descriptors.

### Rate-limit algorithms

Each descriptor defaults to the backwards-compatible fixed window algorithm.
Set `algorithm` to `fixed_window`, `token_bucket`, or `sliding_window` for a
different policy:

```json
{
  "api": [
    {
      "key": "client",
      "value": "example",
      "rate_limit": {
        "algorithm": "token_bucket",
        "unit": "seconds",
        "requests_per_unit": 20
      }
    }
  ]
}
```

The token bucket uses `requests_per_unit` as both its burst capacity and its
refill rate over the selected unit. Sliding-window state is maintained as a
Redis sorted set. Redis updates are atomic, and a request that exactly reaches
the limit is allowed.

### StatsD metrics

Metrics are disabled unless configured. The sink uses a queued UDP client so
metric delivery does not block rate-limit checks:

```yaml
metrics:
  statsd:
    address: statsd:8125
    prefix: steward
    queue_capacity: 1024
```

The service emits request totals, allowed/over-limit counts, configuration
reload/error counts, Redis errors and operation latency, plus the observed
rate-limit value.
