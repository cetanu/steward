use std::time::{SystemTime, UNIX_EPOCH};

use super::scripts::Decision;
use crate::proto::envoy::service::ratelimit::v3::rate_limit_response::{Code, DescriptorStatus};
use crate::rate_limits::{Algorithm, RateLimit};

pub fn duration_until_reset_for(limit: &RateLimit) -> u64 {
    let window_secs = limit.unit.seconds().unwrap_or(60).max(1);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rem = now % window_secs;
    if rem == 0 {
        window_secs
    } else {
        window_secs - rem
    }
}

pub fn limit_remaining_for(limit: &RateLimit, decision: &Decision) -> u32 {
    match limit.algorithm {
        Algorithm::FixedWindow | Algorithm::SlidingWindow => limit
            .requests_per_unit
            .saturating_sub(decision.observed)
            .max(0) as u32,
        Algorithm::TokenBucket => decision.observed.max(0) as u32,
    }
}

pub fn aggregate_descriptor_status(
    limit_decisions: &[(&RateLimit, &Decision)],
) -> DescriptorStatus {
    if limit_decisions.is_empty() {
        return DescriptorStatus {
            code: Code::Ok as i32,
            current_limit: None,
            limit_remaining: 0,
            duration_until_reset: None,
            quota: None,
        };
    }

    let any_over = limit_decisions.iter().any(|(_, dec)| !dec.allowed);

    if any_over {
        // Section 5.2 Rule 2: Over-Limit Status Governing Rule
        // Select violated window with largest duration_until_reset (tie-breaker: shortest unit duration).
        let mut violated: Vec<(&RateLimit, &Decision)> = limit_decisions
            .iter()
            .copied()
            .filter(|(_, dec)| !dec.allowed)
            .collect();

        violated.sort_by(|(l1, _), (l2, _)| {
            let reset1 = duration_until_reset_for(l1);
            let reset2 = duration_until_reset_for(l2);
            let unit_secs1 = l1.unit.seconds().unwrap_or(60);
            let unit_secs2 = l2.unit.seconds().unwrap_or(60);

            reset2
                .cmp(&reset1)
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = violated[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit);

        DescriptorStatus {
            code: Code::OverLimit as i32,
            current_limit: Some(gov_limit.to_proto()),
            limit_remaining: remaining,
            duration_until_reset: Some(prost_types::Duration {
                seconds: reset_secs as i64,
                nanos: 0,
            }),
            quota: None,
        }
    } else {
        // Section 5.2 Rule 3: Allowed Status Governing Rule
        // Select window with lowest ratio of remaining capacity: limit_remaining / capacity
        // Tie-breakers: lowest absolute limit_remaining, then shortest unit duration.
        let mut allowed: Vec<(&RateLimit, &Decision)> = limit_decisions.to_vec();

        allowed.sort_by(|(l1, d1), (l2, d2)| {
            let rem1 = limit_remaining_for(l1, d1);
            let rem2 = limit_remaining_for(l2, d2);
            let cap1 = l1.requests_per_unit.max(1) as u64;
            let cap2 = l2.requests_per_unit.max(1) as u64;

            let ratio_cmp = (rem1 as u128 * cap2 as u128).cmp(&(rem2 as u128 * cap1 as u128));
            let unit_secs1 = l1.unit.seconds().unwrap_or(60);
            let unit_secs2 = l2.unit.seconds().unwrap_or(60);

            ratio_cmp
                .then_with(|| rem1.cmp(&rem2))
                .then_with(|| unit_secs1.cmp(&unit_secs2))
        });

        let (gov_limit, gov_decision) = allowed[0];
        let remaining = limit_remaining_for(gov_limit, gov_decision);
        let reset_secs = duration_until_reset_for(gov_limit);

        DescriptorStatus {
            code: Code::Ok as i32,
            current_limit: Some(gov_limit.to_proto()),
            limit_remaining: remaining,
            duration_until_reset: Some(prost_types::Duration {
                seconds: reset_secs as i64,
                nanos: 0,
            }),
            quota: None,
        }
    }
}
