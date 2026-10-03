use crate::proto::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::RateLimitOverride;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RateLimit {
    #[serde(default)]
    pub algorithm: Algorithm,
    pub unit: Unit,
    pub requests_per_unit: i64,
}

impl From<&RateLimitOverride> for RateLimit {
    fn from(value: &RateLimitOverride) -> Self {
        Self {
            algorithm: Algorithm::FixedWindow,
            requests_per_unit: value.requests_per_unit as i64,
            unit: Unit::from(value.unit),
        }
    }
}

impl From<RateLimitOverride> for RateLimit {
    fn from(value: RateLimitOverride) -> Self {
        Self::from(&value)
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    FixedWindow,
    TokenBucket,
    SlidingWindow,
}

impl Algorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FixedWindow => "fixed_window",
            Self::TokenBucket => "token_bucket",
            Self::SlidingWindow => "sliding_window",
        }
    }

    pub fn short_code(self) -> &'static str {
        match self {
            Self::FixedWindow => "fw",
            Self::TokenBucket => "tb",
            Self::SlidingWindow => "sw",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Unknown,
    #[serde(alias = "second")]
    Seconds,
    #[serde(alias = "minute")]
    Minutes,
    #[serde(alias = "hour")]
    Hours,
    #[serde(alias = "day")]
    Days,
    #[serde(alias = "month")]
    Months,
    #[serde(alias = "year")]
    Years,
}

impl From<i32> for Unit {
    fn from(value: i32) -> Self {
        match value {
            1 => Unit::Seconds,
            2 => Unit::Minutes,
            3 => Unit::Hours,
            4 => Unit::Days,
            5 => Unit::Months,
            6 => Unit::Years,
            _ => Unit::Unknown,
        }
    }
}

impl From<Unit> for usize {
    fn from(value: Unit) -> Self {
        value.seconds().unwrap_or(0) as usize
    }
}

impl From<Unit> for i32 {
    fn from(value: Unit) -> Self {
        value.to_proto()
    }
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Seconds => "seconds",
            Self::Minutes => "minutes",
            Self::Hours => "hours",
            Self::Days => "days",
            Self::Months => "months",
            Self::Years => "years",
        }
    }

    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Unknown => None,
            Self::Seconds => Some(1),
            Self::Minutes => Some(60),
            Self::Hours => Some(3_600),
            Self::Days => Some(86_400),
            Self::Months => Some(2_592_000),
            Self::Years => Some(31_536_000),
        }
    }

    pub fn duration(self) -> Option<Duration> {
        self.seconds().map(Duration::from_secs)
    }

    pub fn to_proto(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Seconds => 1,
            Self::Minutes => 2,
            Self::Hours => 3,
            Self::Days => 4,
            Self::Months => 5,
            Self::Years => 6,
        }
    }
}

impl RateLimit {
    pub fn validate(&self) -> Result<(), String> {
        if self.requests_per_unit <= 0 {
            return Err(format!(
                "invalid requests_per_unit ({}): must be greater than 0",
                self.requests_per_unit
            ));
        }
        if self.requests_per_unit > u32::MAX as i64 {
            return Err(format!(
                "oversized requests_per_unit ({}): exceeds maximum capacity of {}",
                self.requests_per_unit,
                u32::MAX
            ));
        }
        if self.unit == Unit::Unknown || self.unit.seconds().is_none() {
            return Err(format!("invalid rate limit unit: {:?}", self.unit));
        }
        Ok(())
    }

    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    pub fn with_override(&self, override_: &RateLimitOverride) -> Self {
        let mut limit = RateLimit::from(override_);
        limit.algorithm = self.algorithm;
        limit
    }

    pub fn to_proto(
        &self,
    ) -> crate::proto::envoy::service::ratelimit::v3::rate_limit_response::RateLimit {
        crate::proto::envoy::service::ratelimit::v3::rate_limit_response::RateLimit {
            name: String::new(),
            requests_per_unit: self.requests_per_unit.max(0) as u32,
            unit: self.unit.to_proto(),
        }
    }
}

pub fn validate_override(override_: &RateLimitOverride) -> Result<(), &'static str> {
    if override_.requests_per_unit == 0 {
        return Err("rate limit override requests_per_unit must be greater than 0");
    }
    if Unit::from(override_.unit) == Unit::Unknown {
        return Err("rate limit override unit is invalid or unknown");
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DescriptorConfig {
    pub key: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    #[serde(default)]
    pub rate_limits: Option<Vec<RateLimit>>,
    #[serde(default)]
    pub descriptors: Option<Vec<DescriptorConfig>>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub policy_id: Option<String>,
}

impl DescriptorConfig {
    pub fn is_wildcard(&self) -> bool {
        match self.value.as_deref() {
            None | Some("") | Some("*") => true,
            Some(_) => false,
        }
    }

    pub fn collect_rate_limits(&self) -> Vec<RateLimit> {
        let mut limits = Vec::new();
        if let Some(rl) = self.rate_limit {
            limits.push(rl);
        }
        if let Some(ref rls) = self.rate_limits {
            limits.extend(rls.iter().copied());
        }
        limits
    }

    pub fn get_policy_id(&self) -> Option<&str> {
        self.policy_id.as_deref().or(self.id.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyTrieNode {
    pub exact_children: HashMap<String, HashMap<String, PolicyTrieNode>>,
    pub wildcard_children: HashMap<String, PolicyTrieNode>,
    pub rate_limits: Vec<RateLimit>,
    pub policy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult<'a> {
    pub rate_limits: &'a [RateLimit],
    pub policy_id: &'a str,
}

impl PolicyTrieNode {
    pub fn match_entries<'a>(&'a self, entries: &[(&str, &str)]) -> Option<MatchResult<'a>> {
        if entries.is_empty() {
            if !self.rate_limits.is_empty() {
                return Some(MatchResult {
                    rate_limits: &self.rate_limits,
                    policy_id: self.policy_id.as_deref().unwrap_or("default"),
                });
            }
            return None;
        }

        let (key, value) = entries[0];
        let rest = &entries[1..];

        // 1. Exact match takes precedence deterministically
        if let Some(exact_map) = self.exact_children.get(key)
            && let Some(child) = exact_map.get(value)
            && let Some(matched) = child.match_entries(rest)
        {
            return Some(matched);
        }

        // 2. Wildcard match fallback
        if let Some(child) = self.wildcard_children.get(key)
            && let Some(matched) = child.match_entries(rest)
        {
            return Some(matched);
        }

        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyTrie {
    pub root: PolicyTrieNode,
}

impl PolicyTrie {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_descriptors(descriptors: &[DescriptorConfig]) -> Self {
        let mut trie = Self::new();
        for desc in descriptors {
            trie.insert(desc);
        }
        trie
    }

    pub fn insert(&mut self, desc: &DescriptorConfig) {
        Self::insert_into_node(&mut self.root, desc);
    }

    fn insert_into_node(node: &mut PolicyTrieNode, desc: &DescriptorConfig) {
        let child = if desc.is_wildcard() {
            node.wildcard_children.entry(desc.key.clone()).or_default()
        } else {
            let exact_val = desc.value.clone().unwrap();
            node.exact_children
                .entry(desc.key.clone())
                .or_default()
                .entry(exact_val)
                .or_default()
        };

        let limits = desc.collect_rate_limits();
        if !limits.is_empty() {
            for limit in limits {
                if !child.rate_limits.contains(&limit) {
                    child.rate_limits.push(limit);
                }
            }
            if child.policy_id.is_none() {
                child.policy_id = desc.get_policy_id().map(|s| s.to_string());
            }
        }

        if let Some(ref nested) = desc.descriptors {
            for child_desc in nested {
                Self::insert_into_node(child, child_desc);
            }
        }
    }

    pub fn match_entries<'a>(&'a self, entries: &[(&str, &str)]) -> Option<MatchResult<'a>> {
        if entries.is_empty() {
            return None;
        }
        self.root.match_entries(entries)
    }
}

pub fn encode_canonical_path<'a, I>(entries: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    entries
        .into_iter()
        .map(|(k, v)| format!("{}:{k}={}:{v}", k.len(), v.len()))
        .collect::<Vec<_>>()
        .join("/")
}

pub fn rate_limit_key(
    domain: &str,
    policy_id: &str,
    encoded_path: &str,
    limit: &RateLimit,
) -> String {
    let algo_code = limit.algorithm.short_code();
    let unit_seconds = limit.unit.seconds().unwrap_or(60);
    let unit_spec = format!("{unit_seconds}s");
    format!("steward:{{{domain}}}:v1:{policy_id}:{encoded_path}:{algo_code}:{unit_spec}")
}

#[cfg(test)]
mod tests {
    use super::{
        Algorithm, DescriptorConfig, PolicyTrie, RateLimit, Unit, encode_canonical_path,
        rate_limit_key,
    };

    #[test]
    fn defaults_to_fixed_window() {
        let limit: RateLimit =
            serde_json::from_str(r#"{"unit":"seconds","requests_per_unit":10}"#).unwrap();

        assert_eq!(limit.algorithm, Algorithm::FixedWindow);
        assert_eq!(limit.unit, Unit::Seconds);
    }

    #[test]
    fn rejects_invalid_limits() {
        let limit = RateLimit {
            algorithm: Algorithm::TokenBucket,
            unit: Unit::Unknown,
            requests_per_unit: 0,
        };

        assert!(!limit.is_valid());
    }

    #[test]
    fn ordering_sensitivity() {
        // [(A, 1), (B, 2)] != [(B, 2), (A, 1)]
        let rule = DescriptorConfig {
            key: "A".to_string(),
            value: Some("1".to_string()),
            rate_limit: None,
            rate_limits: None,
            descriptors: Some(vec![DescriptorConfig {
                key: "B".to_string(),
                value: Some("2".to_string()),
                rate_limit: Some(RateLimit {
                    algorithm: Algorithm::FixedWindow,
                    unit: Unit::Seconds,
                    requests_per_unit: 10,
                }),
                rate_limits: None,
                descriptors: None,
                id: None,
                policy_id: None,
            }]),
            id: None,
            policy_id: None,
        };

        let trie = PolicyTrie::from_descriptors(&[rule]);

        // Exact order matches
        let matched = trie.match_entries(&[("A", "1"), ("B", "2")]);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().rate_limits[0].requests_per_unit, 10);

        // Reversed order does NOT match
        assert!(trie.match_entries(&[("B", "2"), ("A", "1")]).is_none());

        // Partial prefix does NOT match (interior branch node has no rate limit)
        assert!(trie.match_entries(&[("A", "1")]).is_none());

        // Partial suffix does NOT match
        assert!(trie.match_entries(&[("B", "2")]).is_none());

        // Extra entry does NOT match
        assert!(
            trie.match_entries(&[("A", "1"), ("B", "2"), ("C", "3")])
                .is_none()
        );
    }

    #[test]
    fn parent_scope_isolation() {
        // tenant: acme -> route: /pay (10/min)
        // tenant: globex -> (no children)
        let acme_rule = DescriptorConfig {
            key: "tenant".to_string(),
            value: Some("acme".to_string()),
            rate_limit: None,
            rate_limits: None,
            descriptors: Some(vec![DescriptorConfig {
                key: "route".to_string(),
                value: Some("/pay".to_string()),
                rate_limit: Some(RateLimit {
                    algorithm: Algorithm::FixedWindow,
                    unit: Unit::Minutes,
                    requests_per_unit: 10,
                }),
                rate_limits: None,
                descriptors: None,
                id: None,
                policy_id: Some("pol_pay".to_string()),
            }]),
            id: None,
            policy_id: None,
        };

        let globex_rule = DescriptorConfig {
            key: "tenant".to_string(),
            value: Some("globex".to_string()),
            rate_limit: None,
            rate_limits: None,
            descriptors: None,
            id: None,
            policy_id: None,
        };

        let trie = PolicyTrie::from_descriptors(&[acme_rule, globex_rule]);

        // Acme pays matches Acme rule
        let acme_match = trie.match_entries(&[("tenant", "acme"), ("route", "/pay")]);
        assert!(acme_match.is_some());
        assert_eq!(acme_match.unwrap().policy_id, "pol_pay");

        // Globex pays does NOT leak or inherit Acme's rule
        assert!(
            trie.match_entries(&[("tenant", "globex"), ("route", "/pay")])
                .is_none()
        );
    }

    #[test]
    fn exact_vs_wildcard_precedence() {
        // Table in docs/accounting-contract.md Section 4.1:
        // Rule 1: (tenant, acme) -> (route, /pay): 5 req/s
        // Rule 2: (tenant, acme) -> (route, *): 50 req/s
        // Rule 3: (tenant, *) -> (route, /pay): 20 req/s
        // Rule 4: (tenant, *) -> (route, *): 100 req/s
        let r1 = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 5,
        };
        let r2 = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 50,
        };
        let r3 = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 20,
        };
        let r4 = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 100,
        };

        let rules = vec![
            DescriptorConfig {
                key: "tenant".to_string(),
                value: Some("acme".to_string()),
                rate_limit: None,
                rate_limits: None,
                descriptors: Some(vec![
                    DescriptorConfig {
                        key: "route".to_string(),
                        value: Some("/pay".to_string()),
                        rate_limit: Some(r1),
                        rate_limits: None,
                        descriptors: None,
                        id: Some("r1".to_string()),
                        policy_id: None,
                    },
                    DescriptorConfig {
                        key: "route".to_string(),
                        value: Some("*".to_string()),
                        rate_limit: Some(r2),
                        rate_limits: None,
                        descriptors: None,
                        id: Some("r2".to_string()),
                        policy_id: None,
                    },
                ]),
                id: None,
                policy_id: None,
            },
            DescriptorConfig {
                key: "tenant".to_string(),
                value: None, // Wildcard omitted value
                rate_limit: None,
                rate_limits: None,
                descriptors: Some(vec![
                    DescriptorConfig {
                        key: "route".to_string(),
                        value: Some("/pay".to_string()),
                        rate_limit: Some(r3),
                        rate_limits: None,
                        descriptors: None,
                        id: Some("r3".to_string()),
                        policy_id: None,
                    },
                    DescriptorConfig {
                        key: "route".to_string(),
                        value: Some("".to_string()), // Wildcard empty value
                        rate_limit: Some(r4),
                        rate_limits: None,
                        descriptors: None,
                        id: Some("r4".to_string()),
                        policy_id: None,
                    },
                ]),
                id: None,
                policy_id: None,
            },
        ];

        let trie = PolicyTrie::from_descriptors(&rules);

        // Case 1: [(tenant, acme), (route, /pay)] -> Rule 1 (5 req/s)
        let m1 = trie.match_entries(&[("tenant", "acme"), ("route", "/pay")]);
        assert_eq!(m1.unwrap().policy_id, "r1");

        // Case 2: [(tenant, acme), (route, /search)] -> Rule 2 (50 req/s)
        let m2 = trie.match_entries(&[("tenant", "acme"), ("route", "/search")]);
        assert_eq!(m2.unwrap().policy_id, "r2");

        // Case 3: [(tenant, globex), (route, /pay)] -> Rule 3 (20 req/s)
        let m3 = trie.match_entries(&[("tenant", "globex"), ("route", "/pay")]);
        assert_eq!(m3.unwrap().policy_id, "r3");

        // Case 4: [(tenant, globex), (route, /search)] -> Rule 4 (100 req/s)
        let m4 = trie.match_entries(&[("tenant", "globex"), ("route", "/search")]);
        assert_eq!(m4.unwrap().policy_id, "r4");
    }

    #[test]
    fn dynamic_counter_key_for_wildcards() {
        let rule = DescriptorConfig {
            key: "remote_address".to_string(),
            value: None, // Wildcard
            rate_limit: Some(RateLimit {
                algorithm: Algorithm::FixedWindow,
                unit: Unit::Seconds,
                requests_per_unit: 50,
            }),
            rate_limits: None,
            descriptors: None,
            id: None,
            policy_id: Some("default".to_string()),
        };

        let trie = PolicyTrie::from_descriptors(&[rule]);

        let entries1 = [("remote_address", "192.168.1.1")];
        let m1 = trie.match_entries(&entries1).unwrap();
        let path1 = encode_canonical_path(entries1);
        let key1 = rate_limit_key("default", m1.policy_id, &path1, &m1.rate_limits[0]);
        assert_eq!(
            key1,
            "steward:{default}:v1:default:14:remote_address=11:192.168.1.1:fw:1s"
        );

        let entries2 = [("remote_address", "10.0.0.1")];
        let m2 = trie.match_entries(&entries2).unwrap();
        let path2 = encode_canonical_path(entries2);
        let key2 = rate_limit_key("default", m2.policy_id, &path2, &m2.rate_limits[0]);
        assert_eq!(
            key2,
            "steward:{default}:v1:default:14:remote_address=8:10.0.0.1:fw:1s"
        );

        // Keys are strictly distinct per dynamic value
        assert_ne!(key1, key2);
    }

    #[test]
    fn multiple_limits_on_one_descriptor_path() {
        let rule = DescriptorConfig {
            key: "protect_the_headers_api".to_string(),
            value: Some("1".to_string()),
            rate_limit: None,
            rate_limits: Some(vec![
                RateLimit {
                    algorithm: Algorithm::FixedWindow,
                    unit: Unit::Seconds,
                    requests_per_unit: 5,
                },
                RateLimit {
                    algorithm: Algorithm::FixedWindow,
                    unit: Unit::Minutes,
                    requests_per_unit: 100,
                },
            ]),
            descriptors: None,
            id: None,
            policy_id: Some("pol_headers".to_string()),
        };

        let trie = PolicyTrie::from_descriptors(&[rule]);
        let entries = [("protect_the_headers_api", "1")];
        let m = trie.match_entries(&entries).unwrap();
        assert_eq!(m.rate_limits.len(), 2);
        assert_eq!(m.rate_limits[0].unit, Unit::Seconds);
        assert_eq!(m.rate_limits[0].requests_per_unit, 5);
        assert_eq!(m.rate_limits[1].unit, Unit::Minutes);
        assert_eq!(m.rate_limits[1].requests_per_unit, 100);

        let path = encode_canonical_path(entries);
        let key_sec = rate_limit_key("default", m.policy_id, &path, &m.rate_limits[0]);
        let key_min = rate_limit_key("default", m.policy_id, &path, &m.rate_limits[1]);

        assert_eq!(
            key_sec,
            "steward:{default}:v1:pol_headers:23:protect_the_headers_api=1:1:fw:1s"
        );
        assert_eq!(
            key_min,
            "steward:{default}:v1:pol_headers:23:protect_the_headers_api=1:1:fw:60s"
        );
        assert_ne!(key_sec, key_min);
    }

    #[test]
    fn canonical_key_format_matches_accounting_contract() {
        // Test concrete examples from Section 7.3 of docs/accounting-contract.md:
        // Fixed Window (Exact match, 1-minute window):
        // steward:{default}:v1:pol_pay:6:tenant=4:acme/5:route=4:/pay:fw:60s
        let limit_fw = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Minutes,
            requests_per_unit: 10,
        };
        let entries1 = [("tenant", "acme"), ("route", "/pay")];
        let path1 = encode_canonical_path(entries1);
        let key_fw = rate_limit_key("default", "pol_pay", &path1, &limit_fw);
        assert_eq!(
            key_fw,
            "steward:{default}:v1:pol_pay:6:tenant=4:acme/5:route=4:/pay:fw:60s"
        );

        // Token Bucket (Wildcard matched with user user_891, 1-second window):
        // steward:{default}:v1:pol_api:6:tenant=4:acme/7:user_id=8:user_891:tb:1s
        let limit_tb = RateLimit {
            algorithm: Algorithm::TokenBucket,
            unit: Unit::Seconds,
            requests_per_unit: 100,
        };
        let entries2 = [("tenant", "acme"), ("user_id", "user_891")];
        let path2 = encode_canonical_path(entries2);
        let key_tb = rate_limit_key("default", "pol_api", &path2, &limit_tb);
        assert_eq!(
            key_tb,
            "steward:{default}:v1:pol_api:6:tenant=4:acme/7:user_id=8:user_891:tb:1s"
        );

        // Finding F10: Capacity tuning preserves existing Redis key identity
        let limit_tuned = RateLimit {
            requests_per_unit: 200,
            ..limit_tb
        };
        let key_tuned = rate_limit_key("default", "pol_api", &path2, &limit_tuned);
        assert_eq!(key_tb, key_tuned);
    }

    #[test]
    fn validate_override_rejects_malformed_inputs() {
        use super::validate_override;
        use crate::proto::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::RateLimitOverride;

        // Zero requests per unit rejected
        let zero_capacity = RateLimitOverride {
            requests_per_unit: 0,
            unit: 1, // Seconds
        };
        assert!(validate_override(&zero_capacity).is_err());

        // Unknown unit rejected
        let unknown_unit = RateLimitOverride {
            requests_per_unit: 10,
            unit: 0, // Unknown
        };
        assert!(validate_override(&unknown_unit).is_err());

        // Out-of-range unit rejected
        let invalid_unit = RateLimitOverride {
            requests_per_unit: 10,
            unit: 99,
        };
        assert!(validate_override(&invalid_unit).is_err());

        // Valid override accepted
        let valid = RateLimitOverride {
            requests_per_unit: 50,
            unit: 1, // Seconds
        };
        assert!(validate_override(&valid).is_ok());
    }

    #[test]
    fn changing_capacity_threshold_does_not_alter_counter_key_identity() {
        let base_limit = RateLimit {
            algorithm: Algorithm::FixedWindow,
            unit: Unit::Seconds,
            requests_per_unit: 10,
        };
        let path = encode_canonical_path([("service", "payment")]);
        let key_base = rate_limit_key("default", "policy_1", &path, &base_limit);

        let modified_limit = RateLimit {
            requests_per_unit: 1000,
            ..base_limit
        };
        let key_modified = rate_limit_key("default", "policy_1", &path, &modified_limit);

        assert_eq!(key_base, key_modified);
    }
}
