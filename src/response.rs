use crate::proto::envoy::service::ratelimit::v3::RateLimitResponse;
use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::{Code, DescriptorStatus};

pub fn limit_response(over: bool) -> RateLimitResponse {
    build_response(over, vec![])
}

pub fn build_response(over: bool, statuses: Vec<DescriptorStatus>) -> RateLimitResponse {
    let code = match over {
        true => Code::OverLimit,
        false => Code::Ok,
    };
    RateLimitResponse {
        overall_code: code.into(),
        raw_body: vec![],
        request_headers_to_add: vec![],
        response_headers_to_add: vec![],
        dynamic_metadata: None,
        quota: None,
        statuses,
    }
}
