use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use steward::config_source::{Settings, load_rate_limits, spawn_config_loader};
use steward::metrics::build_metrics;
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
use steward::service::Steward;

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

    info!("loading initial rate-limit configuration");
    let initial_config = load_rate_limits(&settings.rate_limit_configs)
        .await
        .map_err(|error| format!("initial configuration load failed: {error}"))?;

    info!(
        domains = initial_config.len(),
        version_hash = %initial_config.version_hash,
        "initial rate-limit configuration loaded and validated successfully"
    );

    let (config_tx, config_rx) = watch::channel(initial_config);

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
