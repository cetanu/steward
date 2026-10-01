use std::time::Duration;

use reqwest::{Client, StatusCode};
use tokio::time::{Instant, sleep};

const ENVOY_URL: &str = "http://127.0.0.1:8080/headers";

#[tokio::test]
async fn envoy_allows_requests_then_returns_rate_limit_response() {
    let client = Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);

    loop {
        if let Ok(response) = client.get(ENVOY_URL).send().await {
            if response.status().is_success() {
                break;
            }
        }
        assert!(Instant::now() < deadline, "Envoy did not become ready");
        sleep(Duration::from_millis(250)).await;
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
    assert_eq!(body["grpc_status"], "Unavailable");
    assert_eq!(body["message"], "");
}
