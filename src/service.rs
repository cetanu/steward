use crate::db::{self, increment_entry};
use crate::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitService;
use crate::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};
use crate::response::limit_response;
use std::sync::{Arc, Mutex};

use tonic::Response;

// TODO: Make this dynamic
const RATE_LIMIT: i64 = 5;

pub struct Steward {
    cache: Arc<Mutex<redis::Connection>>,
}

impl Steward {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(db::connect("redis").unwrap())),
        }
    }
}

impl Default for Steward {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        let request = dbg!(request.into_inner());
        let mut con = self.cache.lock().unwrap();
        let mut values = vec![];
        for descriptor in request.descriptors.iter() {
            for entry in descriptor.entries.iter() {
                let key = format!("{}_{}_{}", &request.domain, entry.key, entry.value);
                let value = increment_entry(&mut con, &key, &request.hits_addend.max(1));
                values.push(value);
            }
        }
        let rate = values.into_iter().max().unwrap_or(0);
        if rate >= RATE_LIMIT {
            return Ok(Response::new(limit_response(true)));
        }
        Ok(Response::new(limit_response(false)))
    }
}
