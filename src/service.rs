use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error, warn};

use crate::db;
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Descriptor, RateLimit};
use crate::response::limit_response;

pub type RateLimitConfigs = HashMap<String, Vec<Descriptor>>;

pub struct Steward {
    db: Arc<Mutex<db::RedisClient>>,
    rx: Receiver<RateLimitConfigs>,
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
                    database.increment_entry(key, &request.hits_addend.max(1), None)
                }
                Err(e) => {
                    error!("Failed to acquire lock for database: {e}. Setting rate to zero.");
                    0
                }
            });
            let rate = results.max().unwrap_or(0);

            debug!("Reading rate limit configs from config source");
            for limit in rate_limits.iter() {
                let key = format!("{}_{}_{}", &request.domain, limit.key, limit.value);
                if entries.contains(&key) {
                    let rate_limit = limit.rate_limit.requests_per_unit;
                    // match limit.rate_limit.unit {
                    //     Unit::Seconds => (),
                    //     _ => (),
                    // }
                    debug!("Checking if rate ({rate}) is over limit ({rate_limit}) for {key}");

                    // TODO: we need to scale RPS by the unit

                    if rate >= rate_limit {
                        warn!(rate_limit_key=%key, limit=%rate_limit, client_rate=%rate, "Request is over the limit");
                        return Ok(Response::new(limit_response(true)));
                    }
                }
            }
        } else {
            error!("Could not obtain rate limit config from channel");
        }
        Ok(Response::new(limit_response(false)))
    }
}
