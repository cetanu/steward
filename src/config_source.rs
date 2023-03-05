use config::{Config, ConfigError, Environment, File};
use reqwest::Url;
use serde::Deserialize;
use std::{env, net::Ipv4Addr};

use crate::service::RateLimitConfigs;

pub async fn get_config() -> Result<RateLimitConfigs, String> {
    // TODO: no hardcode
    get_http_config(
        "http://mock_config:8000/api/rate_limits"
            .try_into()
            .unwrap(),
    )
    .await
}

pub async fn get_http_config(url: Url) -> Result<RateLimitConfigs, String> {
    match reqwest::get(url).await {
        Ok(response) => Ok(response.json().await.unwrap()),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub enum ConfigSource {
    File(String),
    Http(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenConfig {
    pub addr: Ipv4Addr,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub listen: ListenConfig,
    pub rate_limit_configs: ConfigSource,
    pub redis_host: String,
    pub rate_ttl: usize,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let config_path = env::var("STEWARD_CONFIG_PATH").unwrap_or_else(|_| "steward.yaml".into());

        let mut s = Config::builder();
        for path in config_path.split(',') {
            s = s.add_source(File::with_name(path));
        }
        s = s.add_source(Environment::with_prefix("STEWARD"));
        s.build()?.try_deserialize()
    }
}
