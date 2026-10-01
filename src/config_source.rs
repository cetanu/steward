use config::{Config, ConfigError, Environment, File};
use reqwest::Url;
use serde::Deserialize;
use std::{env, net::Ipv4Addr, time::Duration};

use crate::service::RateLimitConfigs;

pub async fn load_rate_limits(source: &ConfigSource) -> Result<RateLimitConfigs, String> {
    match source {
        ConfigSource::File(path) => load_file_config(path),
        ConfigSource::Http(url) => {
            let url = url
                .parse()
                .map_err(|error| format!("invalid config URL: {error}"))?;
            get_http_config(url).await
        }
    }
}

fn load_file_config(path: &str) -> Result<RateLimitConfigs, String> {
    Config::builder()
        .add_source(File::with_name(path))
        .build()
        .map_err(|error| format!("failed to read rate-limit config: {error}"))?
        .try_deserialize()
        .map_err(|error| format!("failed to parse rate-limit config: {error}"))
}

pub async fn get_http_config(url: Url) -> Result<RateLimitConfigs, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to create config HTTP client: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("config endpoint returned an error: {error}"))?;

    response
        .json()
        .await
        .map_err(|error| format!("invalid rate-limit config response: {error}"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSource {
    File(String),
    Http(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenConfig {
    pub addr: Ipv4Addr,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub listen: ListenConfig,
    pub rate_limit_configs: ConfigSource,
    pub redis_host: String,
    #[serde(default = "default_redis_connections")]
    pub redis_connections: Option<usize>,
    #[serde(default = "default_ttl")]
    pub default_ttl: usize,
    #[serde(default = "default_config_refresh_interval_secs")]
    pub config_refresh_interval_secs: u64,
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    pub statsd: StatsdConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatsdConfig {
    pub address: String,
    #[serde(default = "default_statsd_prefix")]
    pub prefix: String,
    #[serde(default = "default_statsd_queue_capacity")]
    pub queue_capacity: usize,
}

fn default_redis_connections() -> Option<usize> {
    Some(1)
}

fn default_ttl() -> usize {
    10
}

fn default_config_refresh_interval_secs() -> u64 {
    60
}

fn default_statsd_prefix() -> String {
    "steward".to_owned()
}

fn default_statsd_queue_capacity() -> usize {
    1_024
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let config_path = env::var("STEWARD_CONFIG_PATH").unwrap_or_else(|_| "steward.yaml".into());

        let mut builder = Config::builder();
        for path in config_path.split(',') {
            builder = builder.add_source(File::with_name(path));
        }
        builder = builder.add_source(Environment::with_prefix("STEWARD").separator("__"));
        builder.build()?.try_deserialize()
    }
}
