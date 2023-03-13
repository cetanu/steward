use socket2::{Domain, Socket, Type};
use std::collections::HashMap;
use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use tracing::error;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use steward::config_source::{get_http_config, ConfigSource, Settings};
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
use steward::service::Steward;

const MINUTE: Duration = Duration::from_secs(60);

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

    let settings = match Settings::new() {
        Ok(s) => s,
        Err(e) => {
            error!("Could not load config: {e}");
            panic!()
        }
    };

    let (tx, rx) = watch::channel(HashMap::new());

    tokio::spawn(async move {
        loop {
            match settings.rate_limit_configs.clone() {
                ConfigSource::File(_) => {
                    let _ = tx.send(HashMap::new());
                    todo!()
                }
                ConfigSource::Http(url) => {
                    let u = url.as_str().try_into().unwrap();
                    if let Ok(conf) = get_http_config(u).await {
                        let _ = tx.send(conf);
                    }
                }
            }
            // TODO: healthcheck to indicate that the server is ready
            sleep(MINUTE).await;
        }
    });

    let steward = Steward::new(
        settings.redis_host.as_str(),
        settings.rate_ttl,
        rx,
        settings.redis_connections.unwrap_or(1),
    );

    // gRPC server setup
    let addr = SocketAddr::new(
        std::net::IpAddr::V4(settings.listen.addr),
        settings.listen.port,
    );
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.set_nonblocking(true)?;
    socket.listen(128)?; // backlog
    let async_listener = TcpListener::from_std(std::net::TcpListener::from(socket))?;
    let incoming = TcpListenerStream::new(async_listener);
    let service = RateLimitServiceServer::new(steward);
    Server::builder()
        .tcp_keepalive(Some(MINUTE))
        .http2_keepalive_interval(Some(MINUTE))
        .http2_keepalive_timeout(Some(MINUTE))
        .add_service(service)
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}
