use crate::db;
use crate::proto::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::RateLimitOverride;
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::response::limit_response;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::watch::Receiver;
use tonic::Response;

use tracing::{debug, error, warn};

pub type RateLimitConfigs = HashMap<String, Vec<Descriptor>>;

pub struct Steward {
    db: Arc<Mutex<db::RedisClient>>,
    rx: Receiver<RateLimitConfigs>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Unit {
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

#[derive(Serialize, Deserialize)]
pub struct Descriptor {
    key: String,
    value: String,
    rate_limit: RateLimit,
}

#[derive(Serialize, Deserialize)]
struct RateLimit {
    unit: Unit,
    requests_per_unit: i64,
}

impl From<&RateLimitOverride> for RateLimit {
    fn from(value: &RateLimitOverride) -> Self {
        Self {
            requests_per_unit: value.requests_per_unit as i64,
            unit: Unit::from(value.unit),
        }
    }
}

impl Steward {
    pub fn new(redis_host: &str, rate_ttl: usize, rx: Receiver<RateLimitConfigs>) -> Self {
        Self {
            db: Arc::new(Mutex::new(db::RedisClient::new(redis_host, rate_ttl))),
            rx,
        }
    }
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        let request = request.into_inner();
        // debug!(request = ?request, "Received request");
        if let Some(rate_limits) = self.rx.borrow().get(&request.domain) {
            debug!("Loaded rate limits from config source");
            let mut entries = vec![];

            debug!("Reading descriptor entries from request");
            for descriptor in request.descriptors.iter() {
                debug!("Descriptor: {descriptor:?}");

                // TODO: somehow pass the group of entries, along with their overridden limit
                match &descriptor.limit {
                    Some(limit) => debug!(
                        "{:?}",
                        serde_json::to_string(&RateLimit::from(limit)).unwrap()
                    ),
                    None => (),
                }

                for entry in descriptor.entries.iter() {
                    debug!("Entry: {entry:?}");
                    let key = format!("{}_{}_{}", &request.domain, entry.key, entry.value);
                    entries.push(key);
                }
            }

            let results = entries.par_iter().map(|key| match self.db.lock() {
                Ok(mut database) => {
                    debug!("Incrementing entry '{key}' in db");
                    // TODO: support rate limit override

                    // in theory, we have TTLs for seconds, minutes, days
                    // for ttl in TTLS {
                    //     db.increment(key, req, ttl)
                    // }
                    database.increment_entry(&key, &request.hits_addend.max(1), None)
                }
                Err(e) => {
                    error!("Failed to acquire lock for database: {e}");
                    panic!()
                }
            });
            let rate = results.max().unwrap_or(0);

            debug!("Reading descriptor entries from request");
            for limit in rate_limits.iter() {
                let key = format!("{}_{}_{}", &request.domain, limit.key, limit.value);
                if entries.contains(&key) {
                    let rate_limit = limit.rate_limit.requests_per_unit;
                    match limit.rate_limit.unit {
                        Unit::Seconds => (),
                        _ => (),
                    }
                    debug!("Checking if rate ({rate}) is over limit ({rate_limit}) for {key}");

                    // TODO: we need to scale RPS by the unit

                    if rate >= rate_limit {
                        warn!("Request is over the limit");
                        return Ok(Response::new(limit_response(true)));
                    }
                }
            }
        } else {
            error!("Could not obtain config from channel");
        }
        Ok(Response::new(limit_response(false)))
    }
}
