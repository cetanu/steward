use cadence::{
    Counted, Gauged, NopMetricSink, QueuingMetricSink, StatsdClient, Timed, UdpMetricSink,
};
use std::{net::UdpSocket, sync::Arc};
use tracing::warn;

use crate::config_source::MetricsConfig;

pub type SharedMetrics = Arc<StatsdClient>;

pub fn build_metrics(config: Option<&MetricsConfig>) -> Result<SharedMetrics, String> {
    let Some(config) = config else {
        return Ok(Arc::new(StatsdClient::from_sink("", NopMetricSink)));
    };

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to bind StatsD socket: {error}"))?;
    let sink = UdpMetricSink::from(config.statsd.address.as_str(), socket)
        .map_err(|error| format!("failed to resolve StatsD address: {error}"))?;
    let sink = QueuingMetricSink::with_capacity(sink, config.statsd.queue_capacity.max(1));
    let client = StatsdClient::from_sink(config.statsd.prefix.as_str(), sink);

    Ok(Arc::new(client))
}

pub fn count(metrics: &StatsdClient, name: &str, value: i64) {
    if let Err(error) = metrics.count(name, value) {
        warn!(metric = name, %error, "failed to emit StatsD counter");
    }
}

pub fn gauge(metrics: &StatsdClient, name: &str, value: u64) {
    if let Err(error) = metrics.gauge(name, value) {
        warn!(metric = name, %error, "failed to emit StatsD gauge");
    }
}

pub fn time(metrics: &StatsdClient, name: &str, elapsed: std::time::Duration) {
    if let Err(error) = metrics.time(name, elapsed) {
        warn!(metric = name, %error, "failed to emit StatsD timer");
    }
}
