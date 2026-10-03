use config::{Config, ConfigError, Environment, File};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::metrics::{SharedMetrics, count, gauge};
use crate::rate_limits::{DescriptorConfig, PolicyTrie, Unit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledConfig {
    pub version_hash: String,
    pub loaded_at: SystemTime,
    pub domains: HashMap<String, PolicyTrie>,
}

impl CompiledConfig {
    pub fn new(
        version_hash: String,
        loaded_at: SystemTime,
        domains: HashMap<String, PolicyTrie>,
    ) -> Self {
        Self {
            version_hash,
            loaded_at,
            domains,
        }
    }

    pub fn get(&self, domain: &str) -> Option<&PolicyTrie> {
        self.domains.get(domain)
    }

    pub fn len(&self) -> usize {
        self.domains.len()
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    pub fn age_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.loaded_at)
            .unwrap_or_default()
            .as_secs()
    }
}

impl std::ops::Deref for CompiledConfig {
    type Target = HashMap<String, PolicyTrie>;

    fn deref(&self) -> &Self::Target {
        &self.domains
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RawDomainDescriptors {
    Nested { descriptors: Vec<DescriptorConfig> },
    Flat(Vec<DescriptorConfig>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainFileConfig {
    pub domain: String,
    #[serde(default)]
    pub descriptors: Vec<DescriptorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RawRateLimitsConfig {
    SingleDomain(DomainFileConfig),
    DomainList(Vec<DomainFileConfig>),
    DomainMap(HashMap<String, RawDomainDescriptors>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathSegment {
    key: String,
    value: Option<String>,
}

fn normalize_value(desc: &DescriptorConfig) -> Option<String> {
    if desc.is_wildcard() {
        None
    } else {
        desc.value.clone()
    }
}

fn format_path(path: &[PathSegment]) -> String {
    if path.is_empty() {
        return "<root>".to_string();
    }
    path.iter()
        .map(|seg| match &seg.value {
            Some(v) => format!("{}={}", seg.key, v),
            None => format!("{}=*", seg.key),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_descriptor_tree(
    domain: &str,
    current_path: &mut Vec<PathSegment>,
    desc: &DescriptorConfig,
    seen_rules: &mut HashSet<(String, Vec<PathSegment>, Unit)>,
    canonical_rules: &mut Vec<String>,
) -> Result<(), String> {
    if desc.key.trim().is_empty() {
        return Err(format!(
            "descriptor key cannot be empty in domain '{domain}'"
        ));
    }

    current_path.push(PathSegment {
        key: desc.key.clone(),
        value: normalize_value(desc),
    });

    let limits = desc.collect_rate_limits();
    for limit in &limits {
        let path_str = format_path(current_path);

        // 1. Non-positive capacity check
        if limit.requests_per_unit <= 0 {
            return Err(format!(
                "invalid requests_per_unit ({}) for path '{path_str}' in domain '{domain}': must be greater than 0",
                limit.requests_per_unit
            ));
        }

        // 2. Unknown or invalid unit check
        if limit.unit == Unit::Unknown || limit.unit.seconds().is_none() {
            return Err(format!(
                "unknown or invalid unit ({:?}) for path '{path_str}' in domain '{domain}'",
                limit.unit
            ));
        }

        // 3. Oversized capacity check (> u32::MAX)
        if limit.requests_per_unit > u32::MAX as i64 {
            return Err(format!(
                "oversized capacity ({}) for path '{path_str}' in domain '{domain}': exceeds maximum allowed u32::MAX ({})",
                limit.requests_per_unit,
                u32::MAX
            ));
        }

        // 4. Duplicate identical path + unit rule check
        let rule_key = (domain.to_string(), current_path.clone(), limit.unit);
        if !seen_rules.insert(rule_key) {
            return Err(format!(
                "duplicate rate limit rule for path '{path_str}' with unit '{:?}' in domain '{domain}'",
                limit.unit
            ));
        }

        let policy_id = desc.get_policy_id().unwrap_or("default");
        canonical_rules.push(format!(
            "{domain}|{path_str}|{}|{:?}|{}|{policy_id}",
            limit.algorithm.as_str(),
            limit.unit,
            limit.requests_per_unit
        ));
    }

    if let Some(ref nested) = desc.descriptors {
        for child in nested {
            validate_descriptor_tree(domain, current_path, child, seen_rules, canonical_rules)?;
        }
    }

    current_path.pop();
    Ok(())
}

pub fn compile_rate_limits(raw: RawRateLimitsConfig) -> Result<Arc<CompiledConfig>, String> {
    compile_rate_limits_at(raw, SystemTime::now())
}

pub fn compile_rate_limits_at(
    raw: RawRateLimitsConfig,
    loaded_at: SystemTime,
) -> Result<Arc<CompiledConfig>, String> {
    let mut domain_entries: Vec<(String, Vec<DescriptorConfig>)> = Vec::new();
    match raw {
        RawRateLimitsConfig::SingleDomain(single) => {
            domain_entries.push((single.domain, single.descriptors));
        }
        RawRateLimitsConfig::DomainList(list) => {
            for domain_cfg in list {
                domain_entries.push((domain_cfg.domain, domain_cfg.descriptors));
            }
        }
        RawRateLimitsConfig::DomainMap(map) => {
            let mut sorted_keys: Vec<_> = map.keys().cloned().collect();
            sorted_keys.sort();
            for domain in sorted_keys {
                let raw_desc = map.get(&domain).unwrap();
                let descriptors = match raw_desc {
                    RawDomainDescriptors::Nested { descriptors } => descriptors.clone(),
                    RawDomainDescriptors::Flat(descriptors) => descriptors.clone(),
                };
                domain_entries.push((domain, descriptors));
            }
        }
    }

    if domain_entries.is_empty() {
        return Err("configuration contains no domains; rejecting empty snapshot".to_string());
    }

    let mut seen_rules: HashSet<(String, Vec<PathSegment>, Unit)> = HashSet::new();
    let mut canonical_rules: Vec<String> = Vec::new();

    for (domain, descriptors) in &domain_entries {
        if domain.trim().is_empty() {
            return Err("domain name cannot be empty".to_string());
        }
        for desc in descriptors {
            let mut current_path = Vec::new();
            validate_descriptor_tree(
                domain,
                &mut current_path,
                desc,
                &mut seen_rules,
                &mut canonical_rules,
            )?;
        }
    }

    if canonical_rules.is_empty() {
        return Err(
            "configuration contains no rate limit rules; rejecting empty snapshot".to_string(),
        );
    }

    canonical_rules.sort();
    let canonical_joined = canonical_rules.join("\n");
    let digest = ring::digest::digest(&ring::digest::SHA256, canonical_joined.as_bytes());
    let version_hash: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();

    let mut domains: HashMap<String, PolicyTrie> = HashMap::new();
    for (domain, descriptors) in domain_entries {
        let trie = domains.entry(domain).or_default();
        for desc in &descriptors {
            trie.insert(desc);
        }
    }

    Ok(Arc::new(CompiledConfig::new(
        version_hash,
        loaded_at,
        domains,
    )))
}

pub async fn load_rate_limits(source: &ConfigSource) -> Result<Arc<CompiledConfig>, String> {
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

fn load_file_config(path: &str) -> Result<Arc<CompiledConfig>, String> {
    let raw: RawRateLimitsConfig = Config::builder()
        .add_source(File::with_name(path))
        .build()
        .map_err(|error| format!("failed to read rate-limit config: {error}"))?
        .try_deserialize()
        .map_err(|error| format!("failed to parse rate-limit config: {error}"))?;
    compile_rate_limits(raw)
}

pub async fn get_http_config(url: Url) -> Result<Arc<CompiledConfig>, String> {
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
    compile_rate_limits(raw)
}

pub fn spawn_config_loader(
    source: ConfigSource,
    refresh_interval: Duration,
    config_tx: tokio::sync::watch::Sender<Arc<CompiledConfig>>,
    metrics: SharedMetrics,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(refresh_interval).await;
            match load_rate_limits(&source).await {
                Ok(new_config) => {
                    let domains = new_config.len();
                    let version_hash = new_config.version_hash.clone();
                    if config_tx.send(new_config).is_err() {
                        break;
                    }
                    count(&metrics, "config.reloads", 1);
                    gauge(&metrics, "config.age_seconds", 0);
                    tracing::info!(domains, %version_hash, "reloaded rate-limit configuration");
                }
                Err(error) => {
                    count(&metrics, "config.reload_errors", 1);
                    count(&metrics, "config.errors", 1);
                    let active_age = config_tx.borrow().age_seconds();
                    gauge(&metrics, "config.age_seconds", active_age);
                    tracing::warn!(
                        %error,
                        active_version = %config_tx.borrow().version_hash,
                        active_age_seconds = active_age,
                        "failed to reload rate-limit configuration; retaining active version"
                    );
                }
            }
        }
    })
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
    use super::{
        ConfigSource, RawRateLimitsConfig, compile_rate_limits, load_rate_limits,
        spawn_config_loader,
    };
    use crate::rate_limits::Unit;
    use std::time::{Duration, SystemTime};

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
        let configs = compile_rate_limits(raw).unwrap();
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
        let configs = compile_rate_limits(raw).unwrap();
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
        let configs = compile_rate_limits(raw).unwrap();
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

    #[test]
    fn validation_rejects_non_positive_capacity_at_compile_time() {
        let json_zero = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 0 }
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_zero).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("must be greater than 0"),
            "unexpected error message: {err}"
        );

        let json_negative = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": -15 }
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_negative).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("must be greater than 0"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validation_rejects_unknown_units_at_compile_time() {
        let json_unknown = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "unknown", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_unknown).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("unknown or invalid unit"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validation_rejects_oversized_capacity_at_compile_time() {
        let oversized = (u32::MAX as i64) + 1;
        let json_oversized = format!(
            r#"{{
                "domain": "default",
                "descriptors": [
                    {{
                        "key": "k",
                        "value": "v",
                        "rate_limit": {{ "unit": "seconds", "requests_per_unit": {oversized} }}
                    }}
                ]
            }}"#
        );
        let raw: RawRateLimitsConfig = serde_json::from_str(&json_oversized).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(err.contains("exceeds"), "unexpected error message: {err}");
    }

    #[test]
    fn validation_rejects_duplicate_path_and_unit_rules() {
        // Duplicate identical path + unit inside same descriptor
        let json_dup_limits = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limits": [
                        { "unit": "seconds", "requests_per_unit": 5 },
                        { "unit": "seconds", "requests_per_unit": 10 }
                    ]
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_dup_limits).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("duplicate rate limit rule"),
            "unexpected error message: {err}"
        );

        // Duplicate identical path + unit across multiple descriptors
        let json_dup_descs = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "minutes", "requests_per_unit": 5 }
                },
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "minutes", "requests_per_unit": 20 }
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_dup_descs).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("duplicate rate limit rule"),
            "unexpected error message: {err}"
        );

        // Duplicate identical path + unit with wildcards
        let json_dup_wildcard = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "ip",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 5 }
                },
                {
                    "key": "ip",
                    "value": "*",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_dup_wildcard).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("duplicate rate limit rule"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validation_rejects_empty_configuration() {
        let json_empty = r#"{}"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_empty).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("rejecting empty snapshot"),
            "unexpected error message: {err}"
        );

        let json_empty_list = r#"[]"#;
        let raw: RawRateLimitsConfig = serde_json::from_str(json_empty_list).unwrap();
        let err = compile_rate_limits(raw).unwrap_err();
        assert!(
            err.contains("rejecting empty snapshot"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn compilation_generates_stable_version_hash_and_timestamp() {
        let json_str = r#"{
            "default": [
                {
                    "key": "remote_address",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 50 }
                },
                {
                    "key": "route",
                    "value": "/api",
                    "rate_limits": [
                        { "unit": "seconds", "requests_per_unit": 10 },
                        { "unit": "minutes", "requests_per_unit": 100 }
                    ]
                }
            ]
        }"#;

        let before = SystemTime::now();
        let raw1: RawRateLimitsConfig = serde_json::from_str(json_str).unwrap();
        let config1 = compile_rate_limits(raw1).unwrap();
        let after = SystemTime::now();

        let raw2: RawRateLimitsConfig = serde_json::from_str(json_str).unwrap();
        let config2 = compile_rate_limits(raw2).unwrap();

        // Stable version hash across independent compilations of identical config
        assert_eq!(config1.version_hash, config2.version_hash);
        assert_eq!(config1.version_hash.len(), 64); // SHA-256 hex digest

        // Loaded timestamp is bounded
        assert!(config1.loaded_at >= before);
        assert!(config1.loaded_at <= after);

        // Different config produces a different version hash
        let json_diff = r#"{
            "default": [
                {
                    "key": "remote_address",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 51 }
                }
            ]
        }"#;
        let raw_diff: RawRateLimitsConfig = serde_json::from_str(json_diff).unwrap();
        let config_diff = compile_rate_limits(raw_diff).unwrap();
        assert_ne!(config1.version_hash, config_diff.version_hash);
    }

    #[tokio::test]
    async fn reload_failure_preserves_active_configuration_snapshot() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_test_reload_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file_str = file_path.to_str().unwrap().to_string();

        let valid_config_1 = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v1",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;

        std::fs::write(&file_path, valid_config_1).unwrap();

        let config_source = ConfigSource::File(file_str.clone());
        let initial_config = load_rate_limits(&config_source).await.unwrap();
        let initial_hash = initial_config.version_hash.clone();

        let (tx, rx) = tokio::sync::watch::channel(initial_config);
        let metrics =
            std::sync::Arc::new(cadence::StatsdClient::from_sink("", cadence::NopMetricSink));

        // Overwrite config with invalid content (negative capacity)
        let invalid_config = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v1",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": -1 }
                }
            ]
        }"#;
        std::fs::write(&file_path, invalid_config).unwrap();

        // Attempt reload directly
        let reload_res = load_rate_limits(&config_source).await;
        assert!(reload_res.is_err(), "reload should fail on invalid config");

        // Spawn config loader with short interval (40ms)
        let handle = spawn_config_loader(
            config_source.clone(),
            Duration::from_millis(40),
            tx.clone(),
            metrics.clone(),
        );

        // Wait for reload tick to fire and fail
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify watch channel STILL retains initial snapshot untouched
        {
            let active = rx.borrow();
            assert_eq!(active.version_hash, initial_hash);
            assert!(active.get("default").is_some());
            let trie = active.get("default").unwrap();
            assert!(trie.match_entries(&[("k", "v1")]).is_some());
        }

        // Now write a new valid config
        let valid_config_2 = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v2",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 20 }
                }
            ]
        }"#;
        std::fs::write(&file_path, valid_config_2).unwrap();

        // Wait for loader to pick up new valid config
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify watch channel updated to new config
        {
            let active = rx.borrow();
            assert_ne!(active.version_hash, initial_hash);
            let trie = active.get("default").unwrap();
            assert!(trie.match_entries(&[("k", "v2")]).is_some());
        }

        handle.abort();
        let _ = std::fs::remove_file(file_path);
    }
}
