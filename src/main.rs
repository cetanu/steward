use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
use steward::service::Steward;

use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;
use tokio::time::Duration;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

const MINUTE: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let steward = Steward::new();

    // gRPC server setup
    let bind_addr = Ipv4Addr::from([0, 0, 0, 0]);
    let addr = SocketAddr::new(std::net::IpAddr::V4(bind_addr), 5001);
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
