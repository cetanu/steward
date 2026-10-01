use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;

use tokio::{net::TcpListener, sync::watch, time::sleep};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use steward::config_source::{ConfigSource, Settings, load_rate_limits};
use steward::metrics::{SharedMetrics, build_metrics, count};
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
use steward::service::{RateLimitConfigs, Steward};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .event_format(
            tracing_subscriber::fmt::format()
                .with_file(true)
                .with_line_number(true),
        )
        .compact()
        .json()
        .init();

    let settings = Settings::new().map_err(|error| format!("could not load config: {error}"))?;
    let metrics = build_metrics(settings.metrics.as_ref())?;
    let (config_tx, config_rx) = watch::channel(RateLimitConfigs::new());

    spawn_config_loader(
        settings.rate_limit_configs.clone(),
        std::time::Duration::from_secs(settings.config_refresh_interval_secs.max(1)),
        config_tx,
        metrics.clone(),
    );

    let steward = Steward::try_new(
        settings.redis_host.as_str(),
        settings.default_ttl,
        config_rx,
        settings.redis_connections.unwrap_or(1),
        metrics,
    )?;

    let addr = SocketAddr::new(settings.listen.addr.into(), settings.listen.port);
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.set_nonblocking(true)?;
    socket.listen(128)?;
    let listener = TcpListener::from_std(std::net::TcpListener::from(socket))?;
    let incoming = TcpListenerStream::new(listener);

    info!(%addr, "starting Steward rate-limit service");
    Server::builder()
        .tcp_keepalive(Some(std::time::Duration::from_secs(60)))
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(60)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(60)))
        .add_service(RateLimitServiceServer::new(steward))
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}

fn spawn_config_loader(
    source: ConfigSource,
    refresh_interval: std::time::Duration,
    config_tx: watch::Sender<RateLimitConfigs>,
    metrics: SharedMetrics,
) {
    tokio::spawn(async move {
        loop {
            match load_rate_limits(&source).await {
                Ok(config) => {
                    let domains = config.len();
                    if config_tx.send(config).is_err() {
                        break;
                    }
                    count(&metrics, "config.reloads", 1);
                    info!(domains, "loaded rate-limit configuration");
                }
                Err(error) => {
                    count(&metrics, "config.errors", 1);
                    warn!(%error, "failed to load rate-limit configuration; retaining the previous version");
                }
            }
            sleep(refresh_interval).await;
        }
    });
}
