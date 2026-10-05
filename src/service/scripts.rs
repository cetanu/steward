use aws_lc_rs::rand::SecureRandom;
use redis::Script;

const FIXED_WINDOW_SCRIPT: &str = include_str!("../scripts/fixed_window.lua");
const FIXED_WINDOW_REFUND_SCRIPT: &str = include_str!("../scripts/fixed_window_refund.lua");
const TOKEN_BUCKET_SCRIPT: &str = include_str!("../scripts/token_bucket.lua");
const TOKEN_BUCKET_REFUND_SCRIPT: &str = include_str!("../scripts/token_bucket_refund.lua");
const SLIDING_WINDOW_SCRIPT: &str = include_str!("../scripts/sliding_window.lua");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub allowed: bool,
    pub observed: i64,
}

impl redis::FromRedisValue for ScriptOutcome {
    fn from_redis_value(v: redis::Value) -> Result<Self, redis::ParsingError> {
        let (allowed_code, observed): (i64, i64) = redis::FromRedisValue::from_redis_value(v)?;
        Ok(Self {
            allowed: allowed_code == 1,
            observed,
        })
    }
}

pub fn generate_sliding_window_nonce() -> String {
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes)
        .expect("system randomness failed to generate nonce");
    let mut hex = String::with_capacity(32);
    for b in bytes {
        let _ = std::fmt::write(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

#[derive(Clone)]
pub struct StewardScripts {
    pub fixed_window: Script,
    pub fixed_window_refund: Script,
    pub token_bucket: Script,
    pub token_bucket_refund: Script,
    pub sliding_window: Script,
}

impl Default for StewardScripts {
    fn default() -> Self {
        Self {
            fixed_window: Script::new(FIXED_WINDOW_SCRIPT),
            fixed_window_refund: Script::new(FIXED_WINDOW_REFUND_SCRIPT),
            token_bucket: Script::new(TOKEN_BUCKET_SCRIPT),
            token_bucket_refund: Script::new(TOKEN_BUCKET_REFUND_SCRIPT),
            sliding_window: Script::new(SLIDING_WINDOW_SCRIPT),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOperation {
    Consume(u64),
    Probe,
    Refund(u64),
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub allowed: bool,
    pub observed: i64,
}

impl From<ScriptOutcome> for Decision {
    fn from(outcome: ScriptOutcome) -> Self {
        Self {
            allowed: outcome.allowed,
            observed: outcome.observed,
        }
    }
}
