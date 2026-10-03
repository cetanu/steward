use std::net::SocketAddr;

use tokio::sync::watch;
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use std::time::Duration;

use steward::config_source::{Settings, load_initial_rate_limits, spawn_supervised_config_loader};
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
    let startup_budget = Duration::from_secs(settings.startup_timeout_secs);
    let initial_config = load_initial_rate_limits(&settings.rate_limit_configs, startup_budget)
        .await
        .map_err(|error| format!("initial configuration load failed: {error}"))?;

    info!(
        domains = initial_config.len(),
        version_hash = %initial_config.version_hash,
        "initial rate-limit configuration loaded and validated successfully"
    );

    let initial_version_num = {
        let prefix = &initial_config.version_hash[..16.min(initial_config.version_hash.len())];
        u64::from_str_radix(prefix, 16).unwrap_or(0)
    };
    steward::metrics::gauge(&metrics, "config.version", initial_version_num);
    steward::metrics::gauge(&metrics, "config.age_seconds", 0);
    let initial_epoch = initial_config
        .loaded_at
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    steward::metrics::gauge(&metrics, "config.last_reload_timestamp", initial_epoch);
    steward::metrics::gauge(&metrics, "config.consecutive_fetch_failures", 0);
    steward::metrics::gauge(&metrics, "config.stale", 0);

    let (config_tx, config_rx) = watch::channel(initial_config);

    let config_loader_handle = spawn_supervised_config_loader(
        settings.rate_limit_configs.clone(),
        std::time::Duration::from_secs(settings.config_refresh_interval_secs.max(1)),
        std::time::Duration::from_secs(settings.max_stale_duration_secs.max(1)),
        config_tx,
        metrics.clone(),
    );

    let redis_target = settings.redis_target();
    info!(
        redis_target = %steward::service::sanitize_url(&redis_target),
        "connecting to Redis storage backend"
    );

    let steward = Steward::try_new(redis_target.as_str(), config_rx, metrics)
        .await?
        .with_execution_timeout(std::time::Duration::from_millis(
            settings.execution_timeout_ms,
        ))
        .with_max_concurrent_requests(settings.max_concurrent_requests);

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<RateLimitServiceServer<Steward>>()
        .await;
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let addr = SocketAddr::new(settings.listen.addr.into(), settings.listen.port);
    info!(%addr, "starting Steward rate-limit service with gRPC health checking");
    let mut server = Server::builder()
        .concurrency_limit_per_connection(1024)
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
        .tcp_nodelay(true)
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(60)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(60)));

    if let Some(ref tls_settings) = settings.tls {
        if let Some(identity) = tls_settings.load_identity()? {
            let mut tls_config = tonic::transport::ServerTlsConfig::new().identity(identity);
            if let Some(client_ca) = tls_settings.load_client_ca()? {
                info!("enabling mutual TLS (mTLS) caller authentication");
                tls_config = tls_config.client_ca_root(client_ca);
            }
            server = server.tls_config(tls_config)?;
            info!("TLS transport enabled on gRPC server");
        }
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let health_reporter_clone = health_reporter.clone();

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;

        info!("initiating graceful shutdown: marking gRPC health as NOT_SERVING");
        health_reporter_clone
            .set_not_serving::<RateLimitServiceServer<Steward>>()
            .await;
        health_reporter_clone
            .set_service_status("", tonic_health::ServingStatus::NotServing)
            .await;

        // Bounded drain pause: give Envoy/ingress 2 seconds to receive NOT_SERVING and reroute
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let _ = shutdown_tx.send(());
    });

    let serve_future = server
        .add_service(health_service)
        .add_service(RateLimitServiceServer::new(steward))
        .serve_with_shutdown(addr, async {
            let _ = shutdown_rx.await;
            info!("stopping listener and draining in-flight requests");
        });

    if let Err(e) = serve_future.await {
        tracing::error!("server error: {e}");
    }

    info!("draining background tasks and flushing telemetry");
    config_loader_handle.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), config_loader_handle).await;

    info!("Steward graceful shutdown complete");
    Ok(())
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::warn!("failed to install SIGTERM signal handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("received SIGINT (Ctrl+C) shutdown signal");
        }
        _ = terminate => {
            tracing::info!("received SIGTERM shutdown signal");
        }
    }
}
