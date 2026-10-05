use std::time::Duration;

use crate::proto::envoy::extensions::common::ratelimit::v3::RateLimitDescriptor;
use crate::proto::envoy::service::ratelimit::v3::RateLimitRequest;

/// Parse the standard gRPC timeout header (`grpc-timeout` from `request.metadata()`).
/// Standard gRPC timeouts use format `<value><unit>` where unit is `H` (hours),
/// `M` (minutes), `S` (seconds), `m` (milliseconds), `u` (microseconds), or `n` (nanoseconds).
pub fn parse_grpc_timeout(val: &str) -> Option<Duration> {
    let val = val.trim();
    if val.is_empty() {
        return None;
    }
    let mut chars = val.char_indices();
    let (last_idx, last_char) = chars.next_back()?;
    let num_part = &val[..last_idx];
    if num_part.is_empty() {
        return None;
    }
    let num: u64 = num_part.parse().ok()?;
    match last_char {
        'H' => num.checked_mul(3600).map(Duration::from_secs),
        'M' => num.checked_mul(60).map(Duration::from_secs),
        'S' => Some(Duration::from_secs(num)),
        'm' => Some(Duration::from_millis(num)),
        'u' => Some(Duration::from_micros(num)),
        'n' => Some(Duration::from_nanos(num)),
        _ => None,
    }
}

/// Sanitize connection URLs for safe diagnostic logging, redacting passwords/credentials.
pub fn sanitize_url(raw: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(raw) {
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("*****"));
        }
        parsed.to_string()
    } else {
        raw.to_string()
    }
}

/// Normalize Redis connection target to a valid redis://, rediss://, or unix:// URL.
pub fn normalize_redis_url(target: &str) -> Result<String, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("Redis connection target cannot be empty".to_string());
    }
    let url_str = if trimmed.starts_with("redis://")
        || trimmed.starts_with("rediss://")
        || trimmed.starts_with("unix://")
    {
        trimmed.to_string()
    } else {
        format!("redis://{trimmed}")
    };

    url::Url::parse(&url_str)
        .map_err(|e| format!("invalid Redis URL '{}': {e}", sanitize_url(&url_str)))?;

    Ok(url_str)
}

pub fn is_redis_timeout(err: &redis::RedisError) -> bool {
    if err.is_timeout() {
        return true;
    }
    let desc = err.to_string().to_lowercase();
    if desc.contains("failed to acquire redis connection") {
        return false;
    }
    if let Some(detail) = err.detail() {
        let lower = detail.to_lowercase();
        if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline") {
            return true;
        }
    }
    desc.contains("timeout") || desc.contains("timed out") || desc.contains("deadline")
}

pub fn validate_request(request: &RateLimitRequest) -> Result<(), tonic::Status> {
    if request.domain.is_empty() {
        return Err(tonic::Status::invalid_argument("domain cannot be empty"));
    }
    if request.domain.len() > 128 {
        return Err(tonic::Status::invalid_argument(
            "domain length exceeds maximum of 128 bytes",
        ));
    }
    if request.descriptors.len() > 16 {
        return Err(tonic::Status::invalid_argument(
            "descriptors count exceeds maximum of 16",
        ));
    }
    if request.hits_addend > 100 {
        return Err(tonic::Status::invalid_argument(
            "request hits_addend exceeds maximum of 100",
        ));
    }

    for desc in &request.descriptors {
        if desc.entries.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "descriptor must have at least 1 entry",
            ));
        }
        if desc.entries.len() > 8 {
            return Err(tonic::Status::invalid_argument(
                "descriptor entries count exceeds maximum of 8",
            ));
        }
        for entry in &desc.entries {
            if entry.key.is_empty() {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry key cannot be empty",
                ));
            }
            if entry.key.len() > 256 {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry key length exceeds maximum of 256 bytes",
                ));
            }
            if entry.value.len() > 256 {
                return Err(tonic::Status::invalid_argument(
                    "descriptor entry value length exceeds maximum of 256 bytes",
                ));
            }
        }
        if let Some(hits) = desc.hits_addend
            && hits > 100
        {
            return Err(tonic::Status::invalid_argument(format!(
                "descriptor hits_addend ({hits}) exceeds maximum of 100"
            )));
        }
        if let Some(ref override_) = desc.limit
            && let Err(msg) = crate::rate_limits::validate_override(override_)
        {
            return Err(tonic::Status::invalid_argument(msg));
        }
    }

    Ok(())
}

pub fn compute_descriptor_hit_cost(
    desc: &RateLimitDescriptor,
    request_hits_addend: u32,
) -> Result<u64, tonic::Status> {
    let cost = match desc.hits_addend {
        Some(val) => val,
        None if request_hits_addend > 0 => request_hits_addend as u64,
        None => 1,
    };
    if cost > 100 {
        return Err(tonic::Status::invalid_argument(format!(
            "hit cost ({cost}) exceeds maximum of 100"
        )));
    }
    Ok(cost)
}
