use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam::deque::{Injector, Steal};
use rayon::prelude::*;
use tokio::sync::watch::Receiver;
use tonic::Response;
use tracing::{debug, error, info, warn};

use crate::db;
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::rate_limits::{Descriptor, RateLimit};
use crate::response::limit_response;

pub type RateLimitConfigs = HashMap<String, Vec<Descriptor>>;

pub struct Steward {
    rx: Receiver<RateLimitConfigs>,
    db: Arc<Mutex<db::RedisClient>>,
    db_pool: Vec<Arc<Mutex<db::RedisClient>>>,
}

impl Steward {
    pub fn new(
        redis_host: &str,
        default_rate_ttl: usize,
        rx: Receiver<RateLimitConfigs>,
        pool_size: usize,
    ) -> Self {
        let mut db_pool = vec![];
        if pool_size > 1 {
            for _ in 1..pool_size {
                db_pool.push(Self::new_db_connection(redis_host, default_rate_ttl));
            }
        }

        Self {
            db: Self::new_db_connection(redis_host, default_rate_ttl),
            rx,
            db_pool,
        }
    }

    fn acquire_db_connection(&self) -> Result<MutexGuard<db::RedisClient>, String> {
        // TODO: avoid having to iterate all N locks????
        for con in self.db_pool.iter() {
            match con.try_lock() {
                Ok(lock) => return Ok(lock),
                Err(_) => continue,
            };
        }
        match self.db.lock() {
            Ok(l) => Ok(l),
            Err(e) => Err(format!("{e}")),
        }
    }

    fn new_db_connection(redis_host: &str, default_rate_ttl: usize) -> Arc<Mutex<db::RedisClient>> {
        Arc::new(Mutex::new(db::RedisClient::new(
            redis_host,
            default_rate_ttl,
        )))
    }
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        let request = request.into_inner();
        debug!("Received request");
        if let Some(rate_limits) = self.rx.borrow().get(&request.domain) {
            debug!("Loaded rate limits from config source");
            let mut entries = HashMap::with_capacity(request.descriptors.len());

            debug!("Reading descriptor entries from request");
            for descriptor in request.descriptors.iter() {
                debug!("Descriptor: {descriptor:?}");

                let limit_override = descriptor.limit.as_ref().map(RateLimit::from);

                for entry in descriptor.entries.iter() {
                    let key = format!("{}_{}_{}", &request.domain, entry.key, entry.value);
                    for limit in rate_limits.iter() {
                        let requests_per_unit = limit.rate_limit.requests_per_unit;
                        let config_key = format!(
                            "{}_{}_{}_{}_{}",
                            &request.domain,
                            limit.key,
                            limit.value,
                            requests_per_unit,
                            limit.rate_limit.unit.clone() as i32
                        );
                        if config_key.starts_with(&key) {
                            debug!("Rate limit config matches descriptor entry: {config_key}");
                            if let Some(override_) = limit_override.clone() {
                                entries.insert(config_key, override_);
                            } else {
                                entries.insert(config_key, limit.rate_limit.to_owned());
                            }
                        } else {
                            debug!("{key} did not match {config_key}");
                        }
                    }
                }
            }

            let mut results = HashMap::with_capacity(entries.len());
            let injector = Injector::new();

            entries
                .par_iter()
                .for_each(|(key, limit)| match self.acquire_db_connection() {
                    Ok(mut client) => {
                        let injector = &injector;
                        let interval = limit.unit.clone().into();
                        info!("Incrementing entry '{key}' in db");
                        let val = client.increment_entry(
                            key,
                            &request.hits_addend.max(1),
                            Some(interval),
                        );
                        injector.push((key, val));
                    }
                    Err(e) => {
                        error!("Failed to acquire DB connection: {e}");
                        injector.push((key, 0));
                    }
                });

            while let Steal::Success((key, value)) = injector.steal() {
                debug!("Entry {key} has rate of {value}");
                results.insert(key, value);
            }
            debug!("Results: {:?}", results);

            debug!("Checking if any rate limit has been hit");
            for (entry_key, limit) in entries.iter() {
                debug!("Checking if {entry_key} should rate limit");
                let requests_per_unit = limit.requests_per_unit;
                let rate = results.get(entry_key).unwrap();
                info!(
                    "Checking if rate ({rate}) is over limit ({requests_per_unit}) for {entry_key}"
                );
                if rate >= &requests_per_unit {
                    warn!(rate_limit_key=%entry_key, limit=%requests_per_unit, client_rate=%rate, "Request is over the limit");
                    return Ok(Response::new(limit_response(true)));
                }
            }
        } else {
            error!("Could not obtain rate limit config from channel");
        }
        Ok(Response::new(limit_response(false)))
    }
}
