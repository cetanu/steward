use std::time::Duration;

use reqwest::{Client, StatusCode};
use tokio::time::{Instant, sleep};

const ENVOY_URL: &str = "http://127.0.0.1:8080/headers";
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn envoy_allows_requests_then_returns_rate_limit_response() {
    let client = Client::builder()
        .timeout(CLIENT_REQUEST_TIMEOUT)
        .build()
        .expect("build timed reqwest client");

    let timeout_secs = if std::env::var("CI").is_ok() { 60 } else { 2 };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(ENVOY_URL)
            .timeout(CLIENT_REQUEST_TIMEOUT)
            .send()
            .await
            && response.status().is_success()
        {
            ready = true;
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }

    if !ready {
        if std::env::var("CI").is_ok() {
            panic!("Envoy did not become ready");
        } else {
            eprintln!("Skipping integration test: Envoy not reachable at {ENVOY_URL}");
            return;
        }
    }

    // Isolate Redis state if Redis is reachable on default port 6379
    if let Ok(redis_client) = redis::Client::open("redis://127.0.0.1:6379")
        && let Ok(mut conn) = redis_client.get_connection()
    {
        let _: Result<(), _> = redis::cmd("FLUSHDB").query(&mut conn);
    }

    let mut ok_response = None;
    let mut rate_limited_response = None;

    for _ in 0..30 {
        let result = client
            .get(ENVOY_URL)
            .timeout(CLIENT_REQUEST_TIMEOUT)
            .send()
            .await
            .expect("request to Envoy within timeout");

        match result.status() {
            StatusCode::OK => {
                if ok_response.is_none() {
                    ok_response = Some(result);
                }
            }
            StatusCode::TOO_MANY_REQUESTS => {
                rate_limited_response = Some(result);
                break;
            }
            status => {
                panic!("unexpected HTTP status code from Envoy: {status}");
            }
        }
    }

    // 1. Assert successful response headers (HTTP 200)
    let ok = ok_response.expect("expected at least one successful 200 OK request");
    assert_eq!(ok.status(), StatusCode::OK);

    let ok_limit_hdr = ok
        .headers()
        .get("x-ratelimit-limit")
        .expect("x-ratelimit-limit header present on 200 OK");
    let ok_remaining_hdr = ok
        .headers()
        .get("x-ratelimit-remaining")
        .expect("x-ratelimit-remaining header present on 200 OK");
    let ok_reset_hdr = ok
        .headers()
        .get("x-ratelimit-reset")
        .expect("x-ratelimit-reset header present on 200 OK");

    let ok_limit_str = ok_limit_hdr.to_str().expect("valid string limit header");
    let ok_remaining_str = ok_remaining_hdr
        .to_str()
        .expect("valid string remaining header");
    let ok_reset_str = ok_reset_hdr.to_str().expect("valid string reset header");

    assert!(!ok_limit_str.is_empty(), "limit header must not be empty");
    let _rem: u32 = ok_remaining_str
        .parse()
        .expect("remaining must be an integer");
    let reset: u64 = ok_reset_str.parse().expect("reset must be an integer");
    assert!(reset > 0, "reset seconds must be greater than 0");

    // 2. Assert rate-limited response headers and body (HTTP 429)
    let limited = rate_limited_response.expect("repeated requests should be rate limited with 429");
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);

    let lim_limit_hdr = limited
        .headers()
        .get("x-ratelimit-limit")
        .expect("x-ratelimit-limit header present on 429");
    let lim_remaining_hdr = limited
        .headers()
        .get("x-ratelimit-remaining")
        .expect("x-ratelimit-remaining header present on 429");
    let lim_reset_hdr = limited
        .headers()
        .get("x-ratelimit-reset")
        .expect("x-ratelimit-reset header present on 429");

    let lim_limit_str = lim_limit_hdr.to_str().expect("valid string limit header");
    let lim_remaining_str = lim_remaining_hdr
        .to_str()
        .expect("valid string remaining header");
    let lim_reset_str = lim_reset_hdr.to_str().expect("valid string reset header");

    assert!(!lim_limit_str.is_empty(), "limit header must not be empty");
    assert_eq!(
        lim_remaining_str, "0",
        "x-ratelimit-remaining must be 0 on 429"
    );
    let reset: u64 = lim_reset_str.parse().expect("reset must be an integer");
    assert!(reset > 0, "reset seconds must be greater than 0");

    let body: serde_json::Value = limited
        .json()
        .await
        .expect("JSON rate-limit response from Envoy");
    assert_eq!(body["message"], "");
}
