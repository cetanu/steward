use config::{Config, ConfigError, Environment, File};
use reqwest::Url;
use serde::Deserialize;
use std::{collections::HashMap, env, net::Ipv4Addr, time::Duration};

use crate::rate_limits::DescriptorConfig;
use crate::service::RateLimitConfigs;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawDomainDescriptors {
    Nested { descriptors: Vec<DescriptorConfig> },
    Flat(Vec<DescriptorConfig>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct DomainFileConfig {
    pub domain: String,
    #[serde(default)]
    pub descriptors: Vec<DescriptorConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawRateLimitsConfig {
    SingleDomain(DomainFileConfig),
    DomainList(Vec<DomainFileConfig>),
    DomainMap(HashMap<String, RawDomainDescriptors>),
}

pub fn compile_rate_limits(raw: RawRateLimitsConfig) -> RateLimitConfigs {
    let mut configs = RateLimitConfigs::new();
    match raw {
        RawRateLimitsConfig::SingleDomain(single) => {
            let entry = configs.entry(single.domain).or_default();
            for desc in &single.descriptors {
                entry.insert(desc);
            }
        }
        RawRateLimitsConfig::DomainList(list) => {
            for domain_cfg in list {
                let entry = configs.entry(domain_cfg.domain).or_default();
                for desc in &domain_cfg.descriptors {
                    entry.insert(desc);
                }
            }
        }
        RawRateLimitsConfig::DomainMap(map) => {
            for (domain, raw_desc) in map {
                let descriptors = match raw_desc {
                    RawDomainDescriptors::Nested { descriptors } => descriptors,
                    RawDomainDescriptors::Flat(descriptors) => descriptors,
                };
                let entry = configs.entry(domain).or_default();
                for desc in &descriptors {
                    entry.insert(desc);
                }
            }
        }
    }
    configs
}

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
    let raw: RawRateLimitsConfig = Config::builder()
        .add_source(File::with_name(path))
        .build()
        .map_err(|error| format!("failed to read rate-limit config: {error}"))?
        .try_deserialize()
        .map_err(|error| format!("failed to parse rate-limit config: {error}"))?;
    Ok(compile_rate_limits(raw))
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

    let raw: RawRateLimitsConfig = response
        .json()
        .await
        .map_err(|error| format!("invalid rate-limit config response: {error}"))?;
    Ok(compile_rate_limits(raw))
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

#[cfg(test)]
mod tests {
    use super::{RawRateLimitsConfig, compile_rate_limits};
    use crate::rate_limits::Unit;

    #[test]
    fn parses_and_compiles_mock_server_format() {
        let json_str = r#"{
            "default": [
                {
                    "key": "remote_address",
                    "rate_limit": {
                        "unit": "seconds",
                        "requests_per_unit": 50
                    }
                },
                {
                    "key": "protect_the_headers_api",
                    "value": "1",
                    "rate_limits": [
                        {
                            "unit": "seconds",
                            "requests_per_unit": 5
                        },
                        {
                            "unit": "minutes",
                            "requests_per_unit": 100
                        }
                    ]
                }
            ]
        }"#;

        let raw: RawRateLimitsConfig = serde_json::from_str(json_str).unwrap();
        let configs = compile_rate_limits(raw);
        assert!(configs.contains_key("default"));

        let trie = configs.get("default").unwrap();
        // Wildcard IP
        let ip_match = trie.match_entries(&[("remote_address", "1.2.3.4")]);
        assert!(ip_match.is_some());
        assert_eq!(ip_match.unwrap().rate_limits[0].requests_per_unit, 50);

        // Multi-limit headers api
        let headers_match = trie.match_entries(&[("protect_the_headers_api", "1")]);
        assert!(headers_match.is_some());
        let limits = &headers_match.unwrap().rate_limits;
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].unit, Unit::Seconds);
        assert_eq!(limits[0].requests_per_unit, 5);
        assert_eq!(limits[1].unit, Unit::Minutes);
        assert_eq!(limits[1].requests_per_unit, 100);
    }

    #[test]
    fn parses_and_compiles_envoy_single_domain_format() {
        let json_str = r#"{
            "domain": "edge",
            "descriptors": [
                {
                    "key": "tenant",
                    "value": "acme",
                    "descriptors": [
                        {
                            "key": "route",
                            "value": "/pay",
                            "rate_limit": {
                                "unit": "minute",
                                "requests_per_unit": 10
                            }
                        }
                    ]
                }
            ]
        }"#;

        let raw: RawRateLimitsConfig = serde_json::from_str(json_str).unwrap();
        let configs = compile_rate_limits(raw);
        assert!(configs.contains_key("edge"));

        let trie = configs.get("edge").unwrap();
        assert!(
            trie.match_entries(&[("tenant", "acme"), ("route", "/pay")])
                .is_some()
        );
        assert!(trie.match_entries(&[("tenant", "acme")]).is_none());
        assert!(
            trie.match_entries(&[("tenant", "other"), ("route", "/pay")])
                .is_none()
        );
    }

    #[test]
    fn parses_and_compiles_domain_list_format() {
        let json_str = r#"[
            {
                "domain": "d1",
                "descriptors": [
                    {
                        "key": "k1",
                        "value": "v1",
                        "rate_limit": { "unit": "seconds", "requests_per_unit": 5 }
                    }
                ]
            },
            {
                "domain": "d2",
                "descriptors": [
                    {
                        "key": "k2",
                        "value": "v2",
                        "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                    }
                ]
            }
        ]"#;

        let raw: RawRateLimitsConfig = serde_json::from_str(json_str).unwrap();
        let configs = compile_rate_limits(raw);
        assert!(configs.contains_key("d1"));
        assert!(configs.contains_key("d2"));
        assert!(
            configs
                .get("d1")
                .unwrap()
                .match_entries(&[("k1", "v1")])
                .is_some()
        );
        assert!(
            configs
                .get("d2")
                .unwrap()
                .match_entries(&[("k2", "v2")])
                .is_some()
        );
    }
}
