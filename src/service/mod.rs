pub mod reference;
pub mod scripts;
pub mod server;
pub mod status;
pub mod validation;

#[cfg(test)]
mod tests;

pub use reference::*;
pub use scripts::*;
pub use server::*;
pub use status::*;
pub use validation::*;

pub use crate::config_source::CompiledConfig;
pub type RateLimitConfigs = std::sync::Arc<CompiledConfig>;

pub const DEFAULT_EXECUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10);
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 1024;
