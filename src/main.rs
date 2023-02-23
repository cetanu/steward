use steward::proto::envoy::service::ratelimit::v3::rate_limit_response::Code;
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::{
    RateLimitService, RateLimitServiceServer,
};
use steward::proto::envoy::service::ratelimit::v3::{RateLimitRequest, RateLimitResponse};

use redis::{Commands, Value};
use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::time::Duration;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::Response;

const MINUTE: Duration = Duration::from_secs(60);
// TODO: Make these dynamic
const RATE_LIMIT: i64 = 5;
const INTERVAL: usize = 10;

struct Steward {
    cache: Arc<Mutex<redis::Connection>>,
}

fn limit_response(over: bool) -> RateLimitResponse {
    let code = match over {
        true => Code::OverLimit,
        false => Code::Ok,
    };
    RateLimitResponse {
        overall_code: code.into(),
        raw_body: vec![],
        request_headers_to_add: vec![],
        response_headers_to_add: vec![],
        dynamic_metadata: None,
        quota: None,
        statuses: vec![],
    }
}

#[tonic::async_trait]
impl RateLimitService for Steward {
    async fn should_rate_limit(
        &self,
        request: tonic::Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, tonic::Status> {
        let request = dbg!(request.into_inner());

        let mut con = self.cache.lock().unwrap();

        let mut values = vec![];
        for descriptor in request.descriptors.iter() {
            for entry in descriptor.entries.iter() {
                let key = format!("{}_{}_{}", &request.domain, entry.key, entry.value);
                let existed = con.exists(&key);
                let incremented_value = con.incr(&key, request.hits_addend.max(1));

                let value = match incremented_value {
                    Ok(Value::Int(n)) => n,
                    _ => panic!(),
                };
                values.push(value);
                if let Ok(false) = existed {
                    con.expire::<&str, u64>(&key, INTERVAL).unwrap();
                }
            }
        }
        let hits = values.into_iter().max().unwrap_or(0);

        if hits >= RATE_LIMIT {
            return Ok(Response::new(limit_response(true)));
        }

        Ok(Response::new(limit_response(false)))
    }
}

fn connect_to_redis(addr: &str) -> redis::RedisResult<redis::Connection> {
    let client = redis::Client::open(format!("redis://{addr}/"))?;
    client.get_connection()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let steward = Steward {
        cache: Arc::new(Mutex::new(connect_to_redis("redis").unwrap())),
    };

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
