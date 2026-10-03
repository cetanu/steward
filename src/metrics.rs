use cadence::{
    Counted, Gauged, NopMetricSink, QueuingMetricSink, StatsdClient, Timed, UdpMetricSink,
};
use std::{
    net::UdpSocket,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::config_source::MetricsConfig;

pub type SharedMetrics = Arc<StatsdClient>;

static STATSD_QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static STATSD_SEND_ERRORS: AtomicU64 = AtomicU64::new(0);
static STATSD_EMIT_ERRORS: AtomicU64 = AtomicU64::new(0);

static STATSD_DROP_ERROR_LIMITER: ErrorRateLimiter = ErrorRateLimiter::new(5000);
static STATSD_SEND_ERROR_LIMITER: ErrorRateLimiter = ErrorRateLimiter::new(5000);
static STATSD_EMIT_ERROR_LIMITER: ErrorRateLimiter = ErrorRateLimiter::new(5000);

/// Return the total number of metric drops due to a full StatsD queue.
pub fn statsd_queue_drops() -> u64 {
    STATSD_QUEUE_DROPS.load(Ordering::Relaxed)
}

/// Return the total number of UDP transmission errors encountered by the queuing sink worker.
pub fn statsd_send_errors() -> u64 {
    STATSD_SEND_ERRORS.load(Ordering::Relaxed)
}

/// Return the total number of metric emission errors (other than queue full).
pub fn statsd_emit_errors() -> u64 {
    STATSD_EMIT_ERRORS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn reset_statsd_counters() {
    STATSD_QUEUE_DROPS.store(0, Ordering::SeqCst);
    STATSD_SEND_ERRORS.store(0, Ordering::SeqCst);
    STATSD_EMIT_ERRORS.store(0, Ordering::SeqCst);
    STATSD_DROP_ERROR_LIMITER.reset();
    STATSD_SEND_ERROR_LIMITER.reset();
    STATSD_EMIT_ERROR_LIMITER.reset();
}

/// Thread-safe, lock-free rate limiter for logging errors.
pub struct ErrorRateLimiter {
    last_log_ms: AtomicU64,
    suppressed: AtomicU64,
    interval_ms: u64,
}

impl ErrorRateLimiter {
    pub const fn new(interval_ms: u64) -> Self {
        Self {
            last_log_ms: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            interval_ms,
        }
    }

    /// Check if logging is permitted.
    /// Returns `Some(suppressed_count)` if permitted, or `None` if throttled.
    pub fn check(&self) -> Option<u64> {
        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as u64,
            Err(_) => 0,
        };

        let last = self.last_log_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= self.interval_ms
            && self
                .last_log_ms
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
        {
            let suppressed = self.suppressed.swap(0, Ordering::Relaxed);
            return Some(suppressed);
        }

        self.suppressed.fetch_add(1, Ordering::Relaxed);
        None
    }

    #[cfg(test)]
    pub fn reset(&self) {
        self.last_log_ms.store(0, Ordering::SeqCst);
        self.suppressed.store(0, Ordering::SeqCst);
    }
}

pub fn build_metrics(config: Option<&MetricsConfig>) -> Result<SharedMetrics, String> {
    let Some(config) = config else {
        return Ok(Arc::new(StatsdClient::from_sink("", NopMetricSink)));
    };

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to bind StatsD socket: {error}"))?;
    let sink = UdpMetricSink::from(config.statsd.address.as_str(), socket)
        .map_err(|error| format!("failed to resolve StatsD address: {error}"))?;

    let queue_capacity = config.statsd.queue_capacity.max(1);
    let queuing_sink = QueuingMetricSink::builder()
        .with_capacity(queue_capacity)
        .with_error_handler(|err| {
            STATSD_SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
            if let Some(suppressed) = STATSD_SEND_ERROR_LIMITER.check() {
                if suppressed > 0 {
                    tracing::warn!(%err, suppressed, "StatsD UDP metric send failed (some errors suppressed)");
                } else {
                    tracing::warn!(%err, "StatsD UDP metric send failed");
                }
            }
        })
        .build(sink);

    let client = StatsdClient::builder(config.statsd.prefix.as_str(), queuing_sink)
        .with_error_handler(|err| {
            let err_str = err.to_string();
            if err_str.contains("channel full") || err_str.contains("queue full") {
                STATSD_QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                if let Some(suppressed) = STATSD_DROP_ERROR_LIMITER.check() {
                    if suppressed > 0 {
                        tracing::warn!(suppressed, "StatsD metric queue is full; metric dropped (some drops suppressed)");
                    } else {
                        tracing::warn!("StatsD metric queue is full; metric dropped");
                    }
                }
            } else {
                STATSD_EMIT_ERRORS.fetch_add(1, Ordering::Relaxed);
                if let Some(suppressed) = STATSD_EMIT_ERROR_LIMITER.check() {
                    if suppressed > 0 {
                        tracing::warn!(%err, suppressed, "StatsD metric emission failed (some errors suppressed)");
                    } else {
                        tracing::warn!(%err, "StatsD metric emission failed");
                    }
                }
            }
        })
        .build();

    Ok(Arc::new(client))
}

pub fn count(metrics: &StatsdClient, name: &str, value: i64) {
    metrics.count_with_tags(name, value).send();
}

pub fn count_with_tag(
    metrics: &StatsdClient,
    name: &str,
    value: i64,
    tag_key: &str,
    tag_val: &str,
) {
    metrics
        .count_with_tags(name, value)
        .with_tag(tag_key, tag_val)
        .send();
}

pub fn gauge(metrics: &StatsdClient, name: &str, value: u64) {
    metrics.gauge_with_tags(name, value).send();
}

pub fn gauge_with_tag(
    metrics: &StatsdClient,
    name: &str,
    value: u64,
    tag_key: &str,
    tag_val: &str,
) {
    metrics
        .gauge_with_tags(name, value)
        .with_tag(tag_key, tag_val)
        .send();
}

pub fn time(metrics: &StatsdClient, name: &str, elapsed: Duration) {
    metrics.time_with_tags(name, elapsed).send();
}

pub fn time_with_tag(
    metrics: &StatsdClient,
    name: &str,
    elapsed: Duration,
    tag_key: &str,
    tag_val: &str,
) {
    metrics
        .time_with_tags(name, elapsed)
        .with_tag(tag_key, tag_val)
        .send();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn error_rate_limiter_throttles_rapid_invocations() {
        let limiter = ErrorRateLimiter::new(100);
        let first = limiter.check();
        assert!(first.is_some(), "first invocation should be allowed");
        assert_eq!(first.unwrap(), 0, "first invocation has 0 suppressed");

        // Immediate subsequent calls should be suppressed
        for _ in 0..10 {
            assert!(limiter.check().is_none());
        }

        // Wait for interval to elapse
        std::thread::sleep(Duration::from_millis(110));

        let next = limiter.check();
        assert!(
            next.is_some(),
            "subsequent invocation after interval should be allowed"
        );
        assert_eq!(next.unwrap(), 10, "should report 10 suppressed invocations");
    }

    #[test]
    fn statsd_queue_drop_tracking() {
        reset_statsd_counters();

        // Create a queuing sink with capacity 1 wrapping a blocking/dummy sink
        struct FailingSink;
        impl cadence::MetricSink for FailingSink {
            fn emit(&self, _metric: &str) -> std::io::Result<usize> {
                // Return an IO error simulating socket failure
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "connection refused",
                ))
            }
        }

        let queuing = QueuingMetricSink::builder()
            .with_capacity(1)
            .with_error_handler(|_err| {
                STATSD_SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
            })
            .build(FailingSink);

        let client = StatsdClient::builder("test", queuing)
            .with_error_handler(|err| {
                let err_str = err.to_string();
                if err_str.contains("channel full") || err_str.contains("queue full") {
                    STATSD_QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                } else {
                    STATSD_EMIT_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
            })
            .build();

        // Spam metrics rapidly to fill the 1-element queue
        for i in 0..50 {
            count(&client, "test.counter", i);
        }

        // Wait briefly for background thread
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            statsd_queue_drops() > 0 || statsd_send_errors() > 0 || statsd_emit_errors() > 0,
            "error handlers should have tracked drops or send errors"
        );
    }
}
