use crate::proto::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::RateLimitOverride;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Descriptor {
    pub key: String,
    pub value: String,
    pub rate_limit: RateLimit,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RateLimit {
    #[serde(default)]
    pub algorithm: Algorithm,
    pub unit: Unit,
    pub requests_per_unit: i64,
}

impl From<&RateLimitOverride> for RateLimit {
    fn from(value: &RateLimitOverride) -> Self {
        Self {
            algorithm: Algorithm::FixedWindow,
            requests_per_unit: value.requests_per_unit as i64,
            unit: Unit::from(value.unit),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    FixedWindow,
    TokenBucket,
    SlidingWindow,
}

impl Algorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FixedWindow => "fixed_window",
            Self::TokenBucket => "token_bucket",
            Self::SlidingWindow => "sliding_window",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Unknown,
    Seconds,
    Minutes,
    Hours,
    Days,
    Months,
    Years,
}

impl From<i32> for Unit {
    fn from(value: i32) -> Self {
        match value {
            1 => Unit::Seconds,
            2 => Unit::Minutes,
            3 => Unit::Hours,
            4 => Unit::Days,
            5 => Unit::Months,
            6 => Unit::Years,
            _ => Unit::Unknown,
        }
    }
}

impl From<Unit> for usize {
    fn from(value: Unit) -> Self {
        match value {
            Unit::Unknown => 0,
            Unit::Seconds => 1,
            Unit::Minutes => 60,
            Unit::Hours => 3600,
            Unit::Days => 86400,
            Unit::Months => 2592000,
            Unit::Years => 31536000,
        }
    }
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Seconds => "seconds",
            Self::Minutes => "minutes",
            Self::Hours => "hours",
            Self::Days => "days",
            Self::Months => "months",
            Self::Years => "years",
        }
    }

    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Unknown => None,
            Self::Seconds => Some(1),
            Self::Minutes => Some(60),
            Self::Hours => Some(3_600),
            Self::Days => Some(86_400),
            Self::Months => Some(2_592_000),
            Self::Years => Some(31_536_000),
        }
    }

    pub fn duration(self) -> Option<Duration> {
        self.seconds().map(Duration::from_secs)
    }
}

impl RateLimit {
    pub fn is_valid(&self) -> bool {
        self.requests_per_unit > 0 && self.unit.duration().is_some()
    }

    pub fn with_override(&self, override_: &RateLimitOverride) -> Self {
        Self {
            algorithm: self.algorithm,
            requests_per_unit: override_.requests_per_unit as i64,
            unit: Unit::from(override_.unit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Algorithm, RateLimit, Unit};

    #[test]
    fn defaults_to_fixed_window() {
        let limit: RateLimit =
            serde_json::from_str(r#"{"unit":"seconds","requests_per_unit":10}"#).unwrap();

        assert_eq!(limit.algorithm, Algorithm::FixedWindow);
        assert_eq!(limit.unit, Unit::Seconds);
    }

    #[test]
    fn rejects_invalid_limits() {
        let limit = RateLimit {
            algorithm: Algorithm::TokenBucket,
            unit: Unit::Unknown,
            requests_per_unit: 0,
        };

        assert!(!limit.is_valid());
    }
}
