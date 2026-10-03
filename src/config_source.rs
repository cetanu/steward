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
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, canonical_joined.as_bytes());
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

pub const MAX_CONFIG_PAYLOAD_BYTES: usize = 10 * 1024 * 1024; // 10 MiB
pub const DEFAULT_MAX_STALE_DURATION_SECS: u64 = 3600; // 1 hour

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HttpFetchMetadata {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFetchResult {
    /// Newly parsed and validated configuration snapshot
    Modified(Arc<CompiledConfig>),
    /// Server returned 304 Not Modified; configuration is unchanged
    NotModified,
}

pub fn calculate_jittered_interval(base: Duration) -> Duration {
    let base_millis = base.as_millis() as u64;
    if base_millis <= 1 {
        return base;
    }
    // +/- 20% jitter: range is [base * 0.80, base * 1.20]
    let mut buf = [0u8; 8];
    let rng = aws_lc_rs::rand::SystemRandom::new();
    if aws_lc_rs::rand::SecureRandom::fill(&rng, &mut buf).is_err() {
        return base;
    }
    let rand_u64 = u64::from_le_bytes(buf);
    let span = (base_millis as f64) * 0.40;
    let min_millis = (base_millis as f64) * 0.80;
    let fraction = (rand_u64 as f64) / (u64::MAX as f64);
    let jittered_millis = (min_millis + span * fraction).round().max(1.0) as u64;
    Duration::from_millis(jittered_millis)
}

pub async fn load_rate_limits(source: &ConfigSource) -> Result<Arc<CompiledConfig>, String> {
    let mut metadata = HttpFetchMetadata::default();
    match load_rate_limits_with_client(source, None, &mut metadata).await? {
        ConfigFetchResult::Modified(config) => Ok(config),
        ConfigFetchResult::NotModified => {
            Err("unexpected 304 Not Modified on initial configuration load".to_string())
        }
    }
}

pub async fn load_initial_rate_limits(
    source: &ConfigSource,
    startup_budget: Duration,
) -> Result<Arc<CompiledConfig>, String> {
    if startup_budget.is_zero() {
        return load_rate_limits(source).await;
    }
    let start = std::time::Instant::now();
    let mut attempt = 0;
    let mut backoff = Duration::from_millis(250);

    loop {
        attempt += 1;
        match load_rate_limits(source).await {
            Ok(config) => return Ok(config),
            Err(err) => {
                let elapsed = start.elapsed();
                if elapsed >= startup_budget {
                    return Err(format!(
                        "initial configuration load failed after {elapsed:?} ({attempt} attempts): {err}"
                    ));
                }
                let remaining = startup_budget.saturating_sub(elapsed);
                tracing::warn!(
                    attempt,
                    elapsed_ms = elapsed.as_millis(),
                    remaining_ms = remaining.as_millis(),
                    error = %err,
                    "waiting for initial configuration (retrying)..."
                );
                let sleep_duration = backoff.min(remaining).min(Duration::from_secs(2));
                tokio::time::sleep(sleep_duration).await;
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

pub async fn load_rate_limits_with_client(
    source: &ConfigSource,
    http_client: Option<&reqwest::Client>,
    http_metadata: &mut HttpFetchMetadata,
) -> Result<ConfigFetchResult, String> {
    match source {
        ConfigSource::File(path) => {
            let config = load_file_config_async(path.clone()).await?;
            Ok(ConfigFetchResult::Modified(config))
        }
        ConfigSource::Http(url) => {
            let parsed_url = url
                .parse()
                .map_err(|error| format!("invalid config URL: {error}"))?;
            if let Some(client) = http_client {
                fetch_http_config(client, &parsed_url, http_metadata).await
            } else {
                let client = create_http_client()?;
                fetch_http_config(&client, &parsed_url, http_metadata).await
            }
        }
    }
}

pub async fn load_file_config_async(path: String) -> Result<Arc<CompiledConfig>, String> {
    tokio::task::spawn_blocking(move || load_file_config(&path))
        .await
        .map_err(|e| format!("config file loader task panicked or failed: {e}"))?
}

pub fn load_file_config(path: &str) -> Result<Arc<CompiledConfig>, String> {
    let raw: RawRateLimitsConfig = Config::builder()
        .add_source(File::with_name(path))
        .build()
        .map_err(|error| format!("failed to read rate-limit config: {error}"))?
        .try_deserialize()
        .map_err(|error| format!("failed to parse rate-limit config: {error}"))?;
    compile_rate_limits(raw)
}

pub fn create_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to create config HTTP client: {error}"))
}

pub async fn fetch_http_config(
    client: &reqwest::Client,
    url: &Url,
    metadata: &mut HttpFetchMetadata,
) -> Result<ConfigFetchResult, String> {
    let mut req = client.get(url.clone());
    if let Some(ref etag) = metadata.etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(ref last_mod) = metadata.last_modified {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, last_mod);
    }

    let response = req
        .send()
        .await
        .map_err(|error| format!("config request failed: {error}"))?;

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(ConfigFetchResult::NotModified);
    }

    let response = response
        .error_for_status()
        .map_err(|error| format!("config endpoint returned an error: {error}"))?;

    let new_etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let new_last_mod = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let Some(content_length) = response.content_length() {
        if content_length > MAX_CONFIG_PAYLOAD_BYTES as u64 {
            return Err(format!(
                "config response Content-Length ({content_length} bytes) exceeds maximum allowed limit ({MAX_CONFIG_PAYLOAD_BYTES} bytes)"
            ));
        }
    }

    let mut body_bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("failed reading config response chunk: {error}"))?
    {
        if body_bytes.len() + chunk.len() > MAX_CONFIG_PAYLOAD_BYTES {
            return Err(format!(
                "config response payload exceeded maximum allowed limit of {MAX_CONFIG_PAYLOAD_BYTES} bytes"
            ));
        }
        body_bytes.extend_from_slice(&chunk);
    }

    let raw: RawRateLimitsConfig = serde_json::from_slice(&body_bytes)
        .map_err(|error| format!("invalid rate-limit config response: {error}"))?;
    let compiled = compile_rate_limits(raw)?;

    metadata.etag = new_etag;
    metadata.last_modified = new_last_mod;

    Ok(ConfigFetchResult::Modified(compiled))
}

pub async fn get_http_config(url: Url) -> Result<Arc<CompiledConfig>, String> {
    let client = create_http_client()?;
    let mut metadata = HttpFetchMetadata::default();
    match fetch_http_config(&client, &url, &mut metadata).await? {
        ConfigFetchResult::Modified(cfg) => Ok(cfg),
        ConfigFetchResult::NotModified => {
            Err("unexpected 304 Not Modified on initial configuration fetch".to_string())
        }
    }
}

#[derive(Debug, Default)]
struct LoaderState {
    consecutive_failures: u64,
    last_reload_timestamp: u64,
    http_metadata: HttpFetchMetadata,
}

enum LoaderExitReason {
    ChannelClosed,
}

struct WorkerGuard<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for WorkerGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_config_loader_loop(
    source: ConfigSource,
    refresh_interval: Duration,
    max_stale_duration: Duration,
    config_tx: tokio::sync::watch::Sender<Arc<CompiledConfig>>,
    metrics: SharedMetrics,
    http_client: Option<reqwest::Client>,
    shared_state: Arc<tokio::sync::Mutex<LoaderState>>,
) -> LoaderExitReason {
    loop {
        let jittered_duration = calculate_jittered_interval(refresh_interval);
        tokio::time::sleep(jittered_duration).await;

        if config_tx.is_closed() {
            return LoaderExitReason::ChannelClosed;
        }

        let mut http_metadata = {
            let state = shared_state.lock().await;
            state.http_metadata.clone()
        };

        let fetch_result =
            load_rate_limits_with_client(&source, http_client.as_ref(), &mut http_metadata).await;

        let mut state = shared_state.lock().await;
        match fetch_result {
            Ok(ConfigFetchResult::Modified(new_config)) => {
                state.http_metadata = http_metadata;
                state.consecutive_failures = 0;
                let now_epoch = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                state.last_reload_timestamp = now_epoch;

                let domains = new_config.len();
                let version_hash = new_config.version_hash.clone();
                if config_tx.send(new_config).is_err() {
                    return LoaderExitReason::ChannelClosed;
                }
                let version_num = {
                    let prefix = &version_hash[..16.min(version_hash.len())];
                    u64::from_str_radix(prefix, 16).unwrap_or(0)
                };
                count(&metrics, "config.reloads", 1);
                gauge(&metrics, "config.version", version_num);
                gauge(&metrics, "config.age_seconds", 0);
                gauge(&metrics, "config.last_reload_timestamp", now_epoch);
                gauge(&metrics, "config.consecutive_fetch_failures", 0);
                gauge(&metrics, "config.stale", 0);
                tracing::info!(domains, %version_hash, "reloaded rate-limit configuration");
            }
            Ok(ConfigFetchResult::NotModified) => {
                state.consecutive_failures = 0;
                let now_epoch = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                state.last_reload_timestamp = now_epoch;

                count(&metrics, "config.not_modified", 1);
                gauge(&metrics, "config.consecutive_fetch_failures", 0);
                gauge(&metrics, "config.last_reload_timestamp", now_epoch);
                let active_age = config_tx.borrow().age_seconds();
                gauge(&metrics, "config.age_seconds", active_age);
                let is_stale = if active_age > max_stale_duration.as_secs() {
                    1
                } else {
                    0
                };
                gauge(&metrics, "config.stale", is_stale);
                tracing::debug!(
                    active_version = %config_tx.borrow().version_hash,
                    active_age_seconds = active_age,
                    "configuration unchanged (304 Not Modified)"
                );
            }
            Err(error) => {
                state.consecutive_failures += 1;
                let failures = state.consecutive_failures;
                count(&metrics, "config.reload_errors", 1);
                count(&metrics, "config.errors", 1);
                gauge(&metrics, "config.consecutive_fetch_failures", failures);
                let active_age = config_tx.borrow().age_seconds();
                gauge(&metrics, "config.age_seconds", active_age);
                let is_stale = if active_age > max_stale_duration.as_secs() {
                    1
                } else {
                    0
                };
                gauge(&metrics, "config.stale", is_stale);
                if is_stale == 1 {
                    tracing::error!(
                        %error,
                        active_version = %config_tx.borrow().version_hash,
                        active_age_seconds = active_age,
                        max_stale_duration_secs = max_stale_duration.as_secs(),
                        consecutive_failures = failures,
                        "CRITICAL: configuration reload failed and active snapshot exceeds maximum stale duration"
                    );
                } else {
                    tracing::warn!(
                        %error,
                        active_version = %config_tx.borrow().version_hash,
                        active_age_seconds = active_age,
                        consecutive_failures = failures,
                        "failed to reload rate-limit configuration; retaining active version"
                    );
                }
            }
        }
    }
}

pub fn spawn_supervised_config_loader(
    source: ConfigSource,
    refresh_interval: Duration,
    max_stale_duration: Duration,
    config_tx: tokio::sync::watch::Sender<Arc<CompiledConfig>>,
    metrics: SharedMetrics,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let initial_epoch = config_tx
            .borrow()
            .loaded_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let shared_state = Arc::new(tokio::sync::Mutex::new(LoaderState {
            consecutive_failures: 0,
            last_reload_timestamp: initial_epoch,
            http_metadata: HttpFetchMetadata::default(),
        }));

        let http_client = match source {
            ConfigSource::Http(_) => match create_http_client() {
                Ok(client) => Some(client),
                Err(err) => {
                    tracing::error!(%err, "failed to initialize HTTP client for config loader");
                    None
                }
            },
            ConfigSource::File(_) => None,
        };

        loop {
            if config_tx.is_closed() {
                tracing::info!("config watch channel closed; terminating config loader supervisor");
                break;
            }

            let source_clone = source.clone();
            let config_tx_clone = config_tx.clone();
            let metrics_clone = metrics.clone();
            let client_clone = http_client.clone();
            let shared_state_clone = shared_state.clone();

            let worker = tokio::spawn(async move {
                run_config_loader_loop(
                    source_clone,
                    refresh_interval,
                    max_stale_duration,
                    config_tx_clone,
                    metrics_clone,
                    client_clone,
                    shared_state_clone,
                )
                .await
            });

            let mut guard = WorkerGuard(worker);
            match (&mut guard.0).await {
                Ok(LoaderExitReason::ChannelClosed) => {
                    tracing::info!("config channel closed; supervisor exiting");
                    break;
                }
                Err(join_err) => {
                    if join_err.is_cancelled() {
                        tracing::info!("config loader supervisor cancelled; terminating");
                        break;
                    }
                    count(&metrics, "config.loader_crashes", 1);
                    tracing::error!(
                        error = %join_err,
                        "config loader worker task panicked or failed; supervising restart in 1s"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub fn spawn_config_loader(
    source: ConfigSource,
    refresh_interval: Duration,
    config_tx: tokio::sync::watch::Sender<Arc<CompiledConfig>>,
    metrics: SharedMetrics,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_config_loader(
        source,
        refresh_interval,
        Duration::from_secs(DEFAULT_MAX_STALE_DURATION_SECS),
        config_tx,
        metrics,
    )
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
    #[serde(default)]
    pub redis_url: Option<String>,
    #[serde(default = "default_redis_host")]
    pub redis_host: String,
    #[serde(default = "default_redis_connections")]
    pub redis_connections: Option<usize>,
    #[serde(default = "default_ttl")]
    pub default_ttl: usize,
    #[serde(default = "default_config_refresh_interval_secs")]
    pub config_refresh_interval_secs: u64,
    #[serde(default = "default_max_stale_duration_secs")]
    pub max_stale_duration_secs: u64,
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
    #[serde(default = "default_execution_timeout_ms")]
    pub execution_timeout_ms: u64,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
    #[serde(default)]
    pub tls: Option<TlsSettings>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TlsSettings {
    #[serde(default)]
    pub cert_path: Option<String>,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub client_ca_path: Option<String>,
    #[serde(default)]
    pub require_client_auth: bool,
}

impl TlsSettings {
    pub fn is_enabled(&self) -> bool {
        self.cert_path.is_some() && self.key_path.is_some()
            || (env::var("STEWARD_TLS_CERT").is_ok() && env::var("STEWARD_TLS_KEY").is_ok())
            || (env::var("STEWARD_TLS_CERT_PATH").is_ok()
                && env::var("STEWARD_TLS_KEY_PATH").is_ok())
    }

    pub fn load_identity(&self) -> Result<Option<tonic::transport::Identity>, String> {
        let cert_pem = if let Ok(cert) = env::var("STEWARD_TLS_CERT") {
            cert.into_bytes()
        } else if let Ok(path) = env::var("STEWARD_TLS_CERT_PATH") {
            std::fs::read(&path)
                .map_err(|e| format!("failed to read STEWARD_TLS_CERT_PATH '{path}': {e}"))?
        } else if let Some(ref path) = self.cert_path {
            std::fs::read(path).map_err(|e| format!("failed to read cert_path '{path}': {e}"))?
        } else {
            return Ok(None);
        };

        let key_pem = if let Ok(key) = env::var("STEWARD_TLS_KEY") {
            key.into_bytes()
        } else if let Ok(path) = env::var("STEWARD_TLS_KEY_PATH") {
            std::fs::read(&path)
                .map_err(|e| format!("failed to read STEWARD_TLS_KEY_PATH '{path}': {e}"))?
        } else if let Some(ref path) = self.key_path {
            std::fs::read(path).map_err(|e| format!("failed to read key_path '{path}': {e}"))?
        } else {
            return Ok(None);
        };

        Ok(Some(tonic::transport::Identity::from_pem(
            cert_pem, key_pem,
        )))
    }

    pub fn load_client_ca(&self) -> Result<Option<tonic::transport::Certificate>, String> {
        let ca_pem = if let Ok(ca) = env::var("STEWARD_TLS_CLIENT_CA") {
            ca.into_bytes()
        } else if let Ok(path) = env::var("STEWARD_TLS_CLIENT_CA_PATH") {
            std::fs::read(&path)
                .map_err(|e| format!("failed to read STEWARD_TLS_CLIENT_CA_PATH '{path}': {e}"))?
        } else if let Some(ref path) = self.client_ca_path {
            std::fs::read(path)
                .map_err(|e| format!("failed to read client_ca_path '{path}': {e}"))?
        } else {
            return Ok(None);
        };

        Ok(Some(tonic::transport::Certificate::from_pem(ca_pem)))
    }
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

fn default_redis_host() -> String {
    "127.0.0.1:6379".to_string()
}

fn default_execution_timeout_ms() -> u64 {
    10
}

fn default_max_concurrent_requests() -> usize {
    1_024
}

fn default_startup_timeout_secs() -> u64 {
    30
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

fn default_max_stale_duration_secs() -> u64 {
    DEFAULT_MAX_STALE_DURATION_SECS
}

fn default_statsd_prefix() -> String {
    "steward".to_owned()
}

fn default_statsd_queue_capacity() -> usize {
    1_024
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let mut builder = Config::builder();
        let mut file_configured = false;

        if let Ok(config_paths) = env::var("STEWARD_CONFIG_PATH") {
            for path in config_paths.split(',') {
                let trimmed = path.trim();
                if !trimmed.is_empty() {
                    builder = builder.add_source(File::with_name(trimmed));
                    file_configured = true;
                }
            }
        } else {
            let candidate_paths = [
                "./steward.yaml",
                "./steward.yml",
                "/etc/steward/steward.yaml",
                "/etc/steward/steward.yml",
                "/etc/steward.yaml",
                "/etc/steward.yml",
            ];
            for candidate in candidate_paths {
                if std::path::Path::new(candidate).is_file() {
                    builder = builder.add_source(File::with_name(candidate));
                    file_configured = true;
                    break;
                }
            }
        }

        builder = builder.add_source(Environment::with_prefix("STEWARD").separator("__"));

        let built_config = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                if !file_configured {
                    return Err(ConfigError::Message(format!(
                        "No configuration file found at STEWARD_CONFIG_PATH, ./steward.yaml, /etc/steward/steward.yaml, or /etc/steward.yaml, and environment variables were incomplete: {e}"
                    )));
                }
                return Err(e);
            }
        };

        match built_config.try_deserialize() {
            Ok(settings) => Ok(settings),
            Err(err) => {
                if !file_configured {
                    Err(ConfigError::Message(format!(
                        "No configuration file found at STEWARD_CONFIG_PATH, ./steward.yaml, /etc/steward/steward.yaml, or /etc/steward.yaml, and environment variables were incomplete: {err}"
                    )))
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn redis_target(&self) -> String {
        if let Ok(env_url) = env::var("REDIS_URL") {
            if !env_url.trim().is_empty() {
                return env_url.trim().to_string();
            }
        }
        if let Some(ref url) = self.redis_url {
            if !url.trim().is_empty() {
                return url.trim().to_string();
            }
        }
        self.redis_host.clone()
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

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[tokio::test]
    async fn startup_fails_and_stays_unready_on_invalid_config() {
        // 1. Missing configuration file fails initial load
        let missing_source = ConfigSource::File("/non/existent/path/for/config.json".to_string());
        let res_missing = load_rate_limits(&missing_source).await;
        assert!(
            res_missing.is_err(),
            "missing configuration file must fail initial load"
        );

        // 2. Malformed JSON syntax in configuration file fails initial load
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_invalid_syntax_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&file_path, "{ invalid json: syntax error }").unwrap();
        let syntax_source = ConfigSource::File(file_path.to_str().unwrap().to_string());
        let res_syntax = load_rate_limits(&syntax_source).await;
        assert!(
            res_syntax.is_err(),
            "malformed JSON syntax must fail initial load"
        );
        let _ = std::fs::remove_file(&file_path);

        // 3. Invalid rate-limit policy (zero requests_per_unit) fails compile-time validation
        let file_path_val = temp_dir.join(format!(
            "steward_invalid_val_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &file_path_val,
            r#"{"domain": "default", "descriptors": [{"key": "k", "rate_limit": {"unit": "seconds", "requests_per_unit": 0}}]}"#,
        ).unwrap();
        let val_source = ConfigSource::File(file_path_val.to_str().unwrap().to_string());
        let res_val = load_rate_limits(&val_source).await;
        assert!(
            res_val.is_err(),
            "zero requests_per_unit must fail startup validation"
        );
        let _ = std::fs::remove_file(&file_path_val);
    }

    #[test]
    fn settings_redis_target_precedence() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        use super::{ConfigSource, ListenConfig, Settings};

        let mut settings = Settings {
            listen: ListenConfig {
                addr: "0.0.0.0".parse().unwrap(),
                port: 5001,
            },
            rate_limit_configs: ConfigSource::File("test.json".to_string()),
            redis_url: None,
            redis_host: "legacy-host:6379".to_string(),
            redis_connections: Some(1),
            default_ttl: 10,
            config_refresh_interval_secs: 60,
            max_stale_duration_secs: 3600,
            metrics: None,
            execution_timeout_ms: 10,
            max_concurrent_requests: 1024,
            startup_timeout_secs: 30,
            tls: None,
        };

        // 1. Default to redis_host when no REDIS_URL or redis_url
        unsafe { std::env::remove_var("REDIS_URL") };
        assert_eq!(settings.redis_target(), "legacy-host:6379");

        // 2. redis_url takes precedence over redis_host
        settings.redis_url = Some("rediss://default:secret@redis-cluster:6380".to_string());
        assert_eq!(
            settings.redis_target(),
            "rediss://default:secret@redis-cluster:6380"
        );

        // 3. REDIS_URL environment variable takes highest precedence
        unsafe {
            std::env::set_var(
                "REDIS_URL",
                "rediss://env-user:env-pass@env-redis.cloud:6379",
            );
        }
        assert_eq!(
            settings.redis_target(),
            "rediss://env-user:env-pass@env-redis.cloud:6379"
        );
        unsafe { std::env::remove_var("REDIS_URL") };
    }

    #[test]
    fn tls_settings_lifecycle_and_env_overrides() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        use super::TlsSettings;

        let tls = TlsSettings::default();
        assert!(!tls.is_enabled());

        // Test environment variable loading
        let cert_content = "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----";
        let key_content = "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----";
        let ca_content = "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----";

        unsafe {
            std::env::set_var("STEWARD_TLS_CERT", cert_content);
            std::env::set_var("STEWARD_TLS_KEY", key_content);
            std::env::set_var("STEWARD_TLS_CLIENT_CA", ca_content);
        }

        assert!(tls.is_enabled());
        let identity = tls.load_identity().unwrap().expect("identity should load");
        let _ = identity;
        let ca = tls.load_client_ca().unwrap().expect("ca should load");
        let _ = ca;

        unsafe {
            std::env::remove_var("STEWARD_TLS_CERT");
            std::env::remove_var("STEWARD_TLS_KEY");
            std::env::remove_var("STEWARD_TLS_CLIENT_CA");
        }
    }

    #[test]
    fn calculate_jittered_interval_bounds_and_entropy() {
        use super::calculate_jittered_interval;

        let base = Duration::from_millis(1000);
        let mut samples = Vec::new();
        for _ in 0..100 {
            let j = calculate_jittered_interval(base);
            assert!(
                j >= Duration::from_millis(800) && j <= Duration::from_millis(1200),
                "jittered interval ({j:?}) out of expected 800ms-1200ms bounds"
            );
            samples.push(j.as_millis());
        }

        let min_val = samples.iter().copied().min().unwrap();
        let max_val = samples.iter().copied().max().unwrap();
        assert!(
            max_val > min_val,
            "jittered intervals must vary across calls (min={min_val}, max={max_val})"
        );

        // Edge case: zero or 1ms duration returns base without panic
        assert_eq!(
            calculate_jittered_interval(Duration::from_millis(0)),
            Duration::from_millis(0)
        );
        assert_eq!(
            calculate_jittered_interval(Duration::from_millis(1)),
            Duration::from_millis(1)
        );
    }

    #[tokio::test]
    async fn http_conditional_fetch_and_304_not_modified() {
        use super::{ConfigFetchResult, HttpFetchMetadata, create_http_client, fetch_http_config};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url: reqwest::Url = format!("http://127.0.0.1:{port}/config.json")
            .parse()
            .unwrap();

        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req_str = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    if req_str.contains("if-none-match: \"etag-v1\"") {
                        let resp = "HTTP/1.1 304 Not Modified\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else {
                        let body = r#"{"domain":"default","descriptors":[{"key":"k","value":"v","rate_limit":{"unit":"seconds","requests_per_unit":10}}]}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"etag-v1\"\r\nLast-Modified: Sat, 03 Oct 2026 12:00:00 GMT\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        let client = create_http_client().unwrap();
        let mut metadata = HttpFetchMetadata::default();

        // 1. Initial fetch: should return 200 OK with newly compiled config and populate ETag/Last-Modified
        let res1 = fetch_http_config(&client, &url, &mut metadata)
            .await
            .unwrap();
        match res1 {
            ConfigFetchResult::Modified(config) => {
                assert!(config.get("default").is_some());
                assert_eq!(metadata.etag, Some("\"etag-v1\"".to_string()));
                assert_eq!(
                    metadata.last_modified,
                    Some("Sat, 03 Oct 2026 12:00:00 GMT".to_string())
                );
            }
            ConfigFetchResult::NotModified => panic!("initial fetch must not be 304"),
        }

        // 2. Subsequent conditional fetch: sending ETag should yield 304 Not Modified without re-parsing
        let res2 = fetch_http_config(&client, &url, &mut metadata)
            .await
            .unwrap();
        assert_eq!(res2, ConfigFetchResult::NotModified);

        server_handle.abort();
    }

    #[tokio::test]
    async fn http_oversized_content_length_rejected() {
        use super::{HttpFetchMetadata, create_http_client, fetch_http_config};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url: reqwest::Url = format!("http://127.0.0.1:{port}/config.json")
            .parse()
            .unwrap();

        let server_handle = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15728640\r\n\r\n{}";
                let _ = socket.write_all(resp.as_bytes()).await;
            }
        });

        let client = create_http_client().unwrap();
        let mut metadata = HttpFetchMetadata::default();
        let err = fetch_http_config(&client, &url, &mut metadata)
            .await
            .unwrap_err();
        assert!(
            err.contains("exceeds maximum allowed limit"),
            "unexpected error message: {err}"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn http_oversized_streaming_payload_rejected() {
        use super::{HttpFetchMetadata, create_http_client, fetch_http_config};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url: reqwest::Url = format!("http://127.0.0.1:{port}/config.json")
            .parse()
            .unwrap();

        let server_handle = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n")
                    .await;
                let chunk = [b' '; 64 * 1024]; // 64 KiB
                for _ in 0..170 {
                    // 170 * 64 KiB = ~10.8 MiB > 10 MiB limit
                    if socket.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
            }
        });

        let client = create_http_client().unwrap();
        let mut metadata = HttpFetchMetadata::default();
        let err = fetch_http_config(&client, &url, &mut metadata)
            .await
            .unwrap_err();
        assert!(
            err.contains("exceeded maximum allowed limit"),
            "unexpected error message: {err}"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn loader_health_metrics_consecutive_failures_and_staleness() {
        use super::{ConfigSource, load_rate_limits, spawn_supervised_config_loader};

        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_loader_health_{}_{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file_str = file_path.to_str().unwrap().to_string();

        let valid_config = r#"{
            "domain": "default",
            "descriptors": [
                {
                    "key": "k",
                    "value": "v",
                    "rate_limit": { "unit": "seconds", "requests_per_unit": 10 }
                }
            ]
        }"#;
        std::fs::write(&file_path, valid_config).unwrap();

        let config_source = ConfigSource::File(file_str.clone());
        let initial_config = load_rate_limits(&config_source).await.unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(initial_config);
        let metrics =
            std::sync::Arc::new(cadence::StatsdClient::from_sink("", cadence::NopMetricSink));

        // Spawn supervised loader with short refresh (30ms) and short max_stale_duration (1s)
        let handle = spawn_supervised_config_loader(
            config_source.clone(),
            Duration::from_millis(30),
            Duration::from_millis(50),
            tx.clone(),
            metrics.clone(),
        );

        // Break the file to trigger reload failures
        std::fs::write(&file_path, "{ invalid: malformed json }").unwrap();

        // Wait for multiple ticks and staleness threshold to pass
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Restore file to valid config
        std::fs::write(&file_path, valid_config).unwrap();

        // Wait for recovery tick
        tokio::time::sleep(Duration::from_millis(100)).await;

        handle.abort();
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn settings_new_missing_file_and_env_returns_descriptive_error() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        use super::Settings;
        // Ensure STEWARD_CONFIG_PATH is not set for this test
        let prev_config_path = std::env::var("STEWARD_CONFIG_PATH").ok();
        // Clear any STEWARD__* variables that might satisfy configuration
        unsafe {
            std::env::remove_var("STEWARD_CONFIG_PATH");
            std::env::remove_var("STEWARD__LISTEN__ADDR");
            std::env::remove_var("STEWARD__LISTEN__PORT");
            std::env::remove_var("STEWARD__RATE_LIMIT_CONFIGS__FILE");
            std::env::remove_var("STEWARD__RATE_LIMIT_CONFIGS__HTTP");
        }

        let result = Settings::new();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No configuration file found at STEWARD_CONFIG_PATH"),
            "unexpected error message: {err_msg}"
        );

        if let Some(val) = prev_config_path {
            unsafe {
                std::env::set_var("STEWARD_CONFIG_PATH", val);
            }
        }
    }

    #[test]
    fn settings_new_pure_environment_variable_configuration() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        use super::Settings;
        let prev_config_path = std::env::var("STEWARD_CONFIG_PATH").ok();
        unsafe {
            std::env::remove_var("STEWARD_CONFIG_PATH");

            std::env::set_var("STEWARD__LISTEN__ADDR", "127.0.0.1");
            std::env::set_var("STEWARD__LISTEN__PORT", "5099");
            std::env::set_var(
                "STEWARD__RATE_LIMIT_CONFIGS__FILE",
                "/etc/steward/test-limits.json",
            );
            std::env::set_var("STEWARD__REDIS_HOST", "127.0.0.1:6379");
        }

        let settings = Settings::new().expect("should parse pure env config");
        assert_eq!(settings.listen.port, 5099);
        assert_eq!(
            settings.listen.addr,
            "127.0.0.1".parse::<std::net::Ipv4Addr>().unwrap()
        );
        match settings.rate_limit_configs {
            super::ConfigSource::File(path) => {
                assert_eq!(path, "/etc/steward/test-limits.json");
            }
            _ => panic!("expected File config source"),
        }

        unsafe {
            std::env::remove_var("STEWARD__LISTEN__ADDR");
            std::env::remove_var("STEWARD__LISTEN__PORT");
            std::env::remove_var("STEWARD__RATE_LIMIT_CONFIGS__FILE");
            std::env::remove_var("STEWARD__REDIS_HOST");

            if let Some(val) = prev_config_path {
                std::env::set_var("STEWARD_CONFIG_PATH", val);
            }
        }
    }

    #[test]
    fn settings_new_steward_config_path_explicit_override() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        use super::Settings;
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_custom_settings_{}.yaml",
            std::process::id()
        ));
        let content = r#"
listen:
  addr: 127.0.0.1
  port: 5088
rate_limit_configs:
  file: /tmp/limits.json
redis_host: 127.0.0.1:6379
"#;
        std::fs::write(&file_path, content).unwrap();

        let prev_config_path = std::env::var("STEWARD_CONFIG_PATH").ok();
        unsafe {
            std::env::set_var("STEWARD_CONFIG_PATH", file_path.to_str().unwrap());
        }

        let settings = Settings::new().expect("should parse explicit config file");
        assert_eq!(settings.listen.port, 5088);

        let _ = std::fs::remove_file(&file_path);
        unsafe {
            if let Some(val) = prev_config_path {
                std::env::set_var("STEWARD_CONFIG_PATH", val);
            } else {
                std::env::remove_var("STEWARD_CONFIG_PATH");
            }
        }
    }

    #[tokio::test]
    async fn load_initial_rate_limits_succeeds_immediately_for_valid_source() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_initial_load_valid_{}.json",
            std::process::id()
        ));
        let content = r#"{
            "domain": "test_domain",
            "descriptors": [
                {
                    "key": "test_key",
                    "value": "test_val",
                    "rate_limit": {
                        "unit": "seconds",
                        "requests_per_unit": 10
                    }
                }
            ]
        }"#;
        std::fs::write(&file_path, content).unwrap();

        let source = super::ConfigSource::File(file_path.to_str().unwrap().to_string());
        let config = super::load_initial_rate_limits(&source, Duration::from_secs(5))
            .await
            .expect("should load initial config");

        assert_eq!(config.len(), 1);
        assert!(config.get("test_domain").is_some());
        let _ = std::fs::remove_file(&file_path);
    }

    #[tokio::test]
    async fn load_initial_rate_limits_zero_budget_fails_immediately() {
        let source = super::ConfigSource::File("/non/existent/path/for/test.json".to_string());
        let start = std::time::Instant::now();
        let result = super::load_initial_rate_limits(&source, Duration::ZERO).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_millis(500),
            "zero budget should fail immediately"
        );
    }

    #[tokio::test]
    async fn load_initial_rate_limits_times_out_and_reports_attempts() {
        let source = super::ConfigSource::File("/non/existent/path/for/test.json".to_string());
        let start = std::time::Instant::now();
        let result = super::load_initial_rate_limits(&source, Duration::from_millis(600)).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("initial configuration load failed after"),
            "error message should indicate timeout: {err_msg}"
        );
        assert!(
            elapsed >= Duration::from_millis(500),
            "should have waited for budget to expire"
        );
    }

    #[tokio::test]
    async fn load_initial_rate_limits_retries_and_succeeds_when_source_appears() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!(
            "steward_initial_load_delayed_{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&file_path);

        let path_clone = file_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let content = r#"{
                "domain": "delayed_domain",
                "descriptors": [
                    {
                        "key": "delayed_key",
                        "value": "1",
                        "rate_limit": {
                            "unit": "minute",
                            "requests_per_unit": 20
                        }
                    }
                ]
            }"#;
            let _ = std::fs::write(&path_clone, content);
        });

        let source = super::ConfigSource::File(file_path.to_str().unwrap().to_string());
        let config = super::load_initial_rate_limits(&source, Duration::from_secs(3))
            .await
            .expect("should eventually load config after delay");

        assert_eq!(config.len(), 1);
        assert!(config.get("delayed_domain").is_some());
        let _ = std::fs::remove_file(&file_path);
    }
}
