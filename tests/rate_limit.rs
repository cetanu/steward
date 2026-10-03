use std::time::Duration;

use reqwest::{Client, StatusCode};
use tokio::time::{Instant, sleep};

const ENVOY_URL: &str = "http://127.0.0.1:8080/headers";

#[tokio::test]
async fn envoy_allows_requests_then_returns_rate_limit_response() {
    let client = Client::new();
    let timeout_secs = if std::env::var("CI").is_ok() { 60 } else { 2 };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(response) = client.get(ENVOY_URL).send().await
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

    let mut response = None;
    for _ in 0..30 {
        let result = client
            .get(ENVOY_URL)
            .send()
            .await
            .expect("request to Envoy");
        if result.status() == StatusCode::TOO_MANY_REQUESTS {
            response = Some(result);
            break;
        }
    }

    let response = response.expect("repeated requests should be rate limited");
    let body: serde_json::Value = response.json().await.expect("JSON rate-limit response");
    assert_eq!(body["message"], "");
}
