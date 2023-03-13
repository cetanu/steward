steward
============================================================

Mission Statement
------------------------------------------------------------

Steward is an implementation of the Lyft Rate-Limit service.


Features
------------------------------------------------------------

* Load rate limit configs from HTTP or a local file


Building and testing
------------------------------------------------------------

### Prerequisites

* Docker
* Docker-compose
* Make

### Optional prerequisites for building locally

* Rust toolchain


### Building locally

Simple execute `cargo build --release` to create a binary
which, when run, will start the rate limit service as a
gRPC server.

### Running the environment

The environment can be brought up with `make run`.  
It includes an envoy proxy, the rate limit service, a redis
database, a mock configuration server, and a httpbin backend.

### Running tests

The project uses tavern HTTP integration tests.  
They can be executed with `make test`


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
  Http: http://mock_config:8000/api/rate_limits
redis_host: redis
redis_connections: 8
default_ttl: 10
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
