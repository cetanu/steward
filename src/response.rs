use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::Code;
use crate::proto::envoy::service::ratelimit::v3::RateLimitResponse;

pub fn limit_response(over: bool) -> RateLimitResponse {
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
        statuses: vec![],
    }
}
