# Vendored Protobuf Schemas

This directory contains the minimal transitive closure of Protocol Buffer definitions required to compile Envoy's Rate Limit Service (`envoy.service.ratelimit.v3.RateLimitService`) and related message types.

All schemas are vendored and committed to ensure hermetic, deterministic, offline builds without downloading remote archives at build time.

## Upstream Provenance

| Component | Upstream Repository | Tag / Commit | License |
|-----------|---------------------|--------------|---------|
| Envoy API | [envoyproxy/envoy](https://github.com/envoyproxy/envoy) | `v1.39.0` (`8eea3285d6bdb89f8ea34632cfe7ce1608a8f374`) | Apache-2.0 |
| Protocol Buffers | [protocolbuffers/protobuf](https://github.com/protocolbuffers/protobuf) | `v3.21.12` (`f0dc78d7e6e331b8c6bb2d5283e06aa26883ca7c`) | BSD-3-Clause |
| protoc-gen-validate | [bufbuild/protoc-gen-validate](https://github.com/bufbuild/protoc-gen-validate) | `v1.3.3` (`92b9a7df69ca9f71bfc492f7a90adf4d36eab569`) | Apache-2.0 |
| xDS / UDPA API | [cncf/xds](https://github.com/cncf/xds) | `dba9d589def2cd10099a3a64887d859188c2f57a` | Apache-2.0 |

*Note: Previous versions downloaded full zip archives of `googleapis`, `opencensus-proto`, and `client_model` at build time. These repositories are not required by the RLS protobuf import closure and have been excluded.*

## Vendored File Closure (25 Files)

### Envoy (`envoy/`)
- `envoy/service/ratelimit/v3/rls.proto` - Main Rate Limit Service definition
- `envoy/extensions/common/ratelimit/v3/ratelimit.proto` - Rate limit descriptor and override definitions
- `envoy/config/core/v3/address.proto` - Network address configurations
- `envoy/config/core/v3/backoff.proto` - Backoff retry strategy configuration
- `envoy/config/core/v3/base.proto` - Core message types (headers, data sources, etc.)
- `envoy/config/core/v3/extension.proto` - Extension configuration
- `envoy/config/core/v3/http_uri.proto` - HTTP URI specification
- `envoy/config/core/v3/socket_option.proto` - Socket options
- `envoy/type/v3/percent.proto` - Percentage and fractional types
- `envoy/type/v3/ratelimit_unit.proto` - Rate limit time units
- `envoy/type/v3/semantic_version.proto` - Semantic versioning
- `envoy/type/v3/token_bucket.proto` - Token bucket rate limiter configuration
- `envoy/annotations/deprecation.proto` - Field deprecation annotations

### Google Protocol Buffers (`google/protobuf/`)
- `google/protobuf/any.proto` - Well-known Any type
- `google/protobuf/descriptor.proto` - Protobuf descriptor definitions
- `google/protobuf/duration.proto` - Well-known Duration type
- `google/protobuf/struct.proto` - Well-known Struct / Value / NullValue types
- `google/protobuf/timestamp.proto` - Well-known Timestamp type
- `google/protobuf/wrappers.proto` - Well-known primitive wrapper types

### Validate (`validate/`)
- `validate/validate.proto` - PGV validation rule annotations

### UDPA / xDS (`udpa/`, `xds/`)
- `udpa/annotations/migrate.proto` - Migration annotations
- `udpa/annotations/status.proto` - Package status annotations
- `udpa/annotations/versioning.proto` - Previous message type annotations
- `xds/annotations/v3/status.proto` - xDS status annotations
- `xds/core/v3/context_params.proto` - Context parameters
