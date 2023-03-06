use crate::proto::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::RateLimitOverride;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct Descriptor {
    pub key: String,
    pub value: String,
    pub rate_limit: RateLimit,
}

#[derive(Serialize, Deserialize)]
pub struct RateLimit {
    pub unit: Unit,
    pub requests_per_unit: i64,
}

impl From<&RateLimitOverride> for RateLimit {
    fn from(value: &RateLimitOverride) -> Self {
        Self {
            requests_per_unit: value.requests_per_unit as i64,
            unit: Unit::from(value.unit),
        }
    }
}

#[derive(Serialize, Deserialize)]
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
