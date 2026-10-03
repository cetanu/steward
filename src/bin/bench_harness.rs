use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use steward::config_source::{DomainFileConfig, RawRateLimitsConfig, compile_rate_limits};
use steward::proto::envoy::extensions::common::ratelimit::v3::{
    RateLimitDescriptor, rate_limit_descriptor::Entry,
};
use steward::proto::envoy::service::ratelimit::v3::RateLimitRequest;
use steward::proto::envoy::service::ratelimit::v3::rate_limit_response::Code;
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_client::RateLimitServiceClient;
use steward::proto::envoy::service::ratelimit::v3::rate_limit_service_server::RateLimitServiceServer;
use steward::rate_limits::{Algorithm, DescriptorConfig, RateLimit, Unit};
use steward::service::Steward;
use tokio::sync::Semaphore;
use tonic::transport::{Channel, Server};

/// High-speed, lock-free Xorshift64 random generator for reproducible benchmarks.
pub struct FastRng {
    state: u64,
}

impl FastRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9007199254740992.0)
    }

    #[inline]
    pub fn gen_range(&mut self, max: usize) -> usize {
        if max == 0 {
            0
        } else {
            (self.next_u64() as usize) % max
        }
    }
}

/// Precomputed Zipfian distribution generator over N elements with skew parameter s.
pub struct ZipfGenerator {
    n: usize,
    cdf: Vec<f64>,
}

impl ZipfGenerator {
    pub fn new(n: usize, s: f64) -> Self {
        let mut cdf = Vec::with_capacity(n);
        let mut sum = 0.0;
        for i in 1..=n {
            sum += 1.0 / (i as f64).powf(s);
            cdf.push(sum);
        }
        for val in &mut cdf {
            *val /= sum;
        }
        Self { n, cdf }
    }

    #[inline]
    pub fn sample(&self, rng: &mut FastRng) -> usize {
        let u = rng.next_f64();
        match self
            .cdf
            .binary_search_by(|probe| probe.partial_cmp(&u).unwrap())
        {
            Ok(idx) => idx,
            Err(idx) => idx.min(self.n - 1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDistribution {
    Uniform,
    Zipfian,
}

#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub name: String,
    pub domain: String,
    pub algorithm: String,
    pub rule_count: usize,
    pub hit_cost: u64,
    pub distribution: KeyDistribution,
    pub target_qps: usize,
    pub duration_secs: u64,
    pub key_cardinality: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BenchmarkResult {
    pub scenario: String,
    pub target_qps: usize,
    pub offered_qps: f64,
    pub completed_qps: f64,
    pub total_requests: usize,
    pub allowed_count: usize,
    pub denied_count: usize,
    pub error_count: usize,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub p99_9_micros: u64,
    pub max_micros: u64,
    pub service_rss_mb: f64,
    pub redis_rss_mb: f64,
}

pub struct LatencyTracker {
    samples: Vec<u64>,
    allowed: usize,
    denied: usize,
    errors: usize,
}

impl LatencyTracker {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            allowed: 0,
            denied: 0,
            errors: 0,
        }
    }

    pub fn record(&mut self, latency_micros: u64, code: Result<i32, String>) {
        self.samples.push(latency_micros);
        match code {
            Ok(c) if c == Code::Ok as i32 => self.allowed += 1,
            Ok(c) if c == Code::OverLimit as i32 => self.denied += 1,
            _ => self.errors += 1,
        }
    }

    pub fn summarize(
        &mut self,
        scenario: &str,
        target_qps: usize,
        elapsed: Duration,
        service_rss: f64,
        redis_rss: f64,
    ) -> BenchmarkResult {
        let total = self.samples.len();
        if total == 0 {
            return BenchmarkResult {
                scenario: scenario.to_string(),
                target_qps,
                ..Default::default()
            };
        }

        self.samples.sort_unstable();

        let percentile = |p: f64| -> u64 {
            let idx = ((p / 100.0) * (total as f64)).round() as usize;
            self.samples[idx.min(total - 1)]
        };

        BenchmarkResult {
            scenario: scenario.to_string(),
            target_qps,
            offered_qps: (total as f64) / elapsed.as_secs_f64(),
            completed_qps: ((self.allowed + self.denied) as f64) / elapsed.as_secs_f64(),
            total_requests: total,
            allowed_count: self.allowed,
            denied_count: self.denied,
            error_count: self.errors,
            p50_micros: percentile(50.0),
            p95_micros: percentile(95.0),
            p99_micros: percentile(99.0),
            p99_9_micros: percentile(99.9),
            max_micros: *self.samples.last().unwrap_or(&0),
            service_rss_mb: service_rss,
            redis_rss_mb: redis_rss,
        }
    }
}

pub fn read_rss_mb(pid: u32) -> f64 {
    let path = format!("/proc/{pid}/statm");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|pages| pages.parse::<u64>().ok())
        })
        .map(|pages| (pages as f64 * 4096.0) / (1024.0 * 1024.0))
        .unwrap_or(0.0)
}

pub struct DedicatedRedis {
    pub port: u16,
    pub child: Child,
}

impl DedicatedRedis {
    pub fn start(port: u16) -> Result<Self, String> {
        let child = Command::new("redis-server")
            .arg("--port")
            .arg(port.to_string())
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no")
            .arg("--dir")
            .arg("/tmp")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn redis-server on port {port}: {e}"))?;

        std::thread::sleep(Duration::from_millis(300));
        Ok(Self { port, child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for DedicatedRedis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Generate comprehensive multi-domain rate limit configuration for the benchmark suite.
pub fn generate_bench_config() -> RawRateLimitsConfig {
    let mut domain_configs = Vec::new();

    let algorithms = [
        (Algorithm::FixedWindow, "fixed_window"),
        (Algorithm::TokenBucket, "token_bucket"),
        (Algorithm::SlidingWindow, "sliding_window"),
    ];
    let rule_counts = [1, 2, 16];

    for (algo, algo_name) in algorithms {
        for rules in rule_counts {
            let domain = format!("bench_{algo_name}_{rules}");
            let mut descriptors = Vec::new();

            for r in 0..rules {
                descriptors.push(DescriptorConfig {
                    key: format!("dim_{r}"),
                    value: None, // Wildcard dynamic client matching
                    rate_limit: Some(RateLimit {
                        algorithm: algo,
                        unit: Unit::Minutes,
                        requests_per_unit: 10_000_000, // Ample capacity for high-throughput baseline
                    }),
                    rate_limits: None,
                    descriptors: None,
                    id: None,
                    policy_id: None,
                });
            }

            domain_configs.push(DomainFileConfig {
                domain,
                descriptors,
            });
        }
    }

    // Over-limit benchmark domain with strict 100 QPS limit
    domain_configs.push(DomainFileConfig {
        domain: "bench_overlimit".to_string(),
        descriptors: vec![DescriptorConfig {
            key: "action".to_string(),
            value: Some("throttled".to_string()),
            rate_limit: Some(RateLimit {
                algorithm: Algorithm::FixedWindow,
                unit: Unit::Minutes,
                requests_per_unit: 100,
            }),
            rate_limits: None,
            descriptors: None,
            id: None,
            policy_id: None,
        }],
    });

    RawRateLimitsConfig::DomainList(domain_configs)
}

pub fn build_rate_limit_request(
    domain: &str,
    rule_count: usize,
    key_id: usize,
    hit_cost: u64,
) -> RateLimitRequest {
    let descriptors = if domain == "bench_overlimit" {
        vec![RateLimitDescriptor {
            entries: vec![Entry {
                key: "action".to_string(),
                value: "throttled".to_string(),
            }],
            limit: None,
            hits_addend: Some(hit_cost),
            is_negative_hits: false,
        }]
    } else {
        let mut descs = Vec::with_capacity(rule_count);
        for r in 0..rule_count {
            descs.push(RateLimitDescriptor {
                entries: vec![Entry {
                    key: format!("dim_{r}"),
                    value: format!("client_{key_id}"),
                }],
                limit: None,
                hits_addend: Some(hit_cost),
                is_negative_hits: false,
            });
        }
        descs
    };

    RateLimitRequest {
        domain: domain.to_string(),
        descriptors,
        hits_addend: 0,
    }
}

/// Execute an open-loop benchmark against a gRPC channel without coordinated omission.
pub async fn run_grpc_benchmark(
    channel: Channel,
    scenario: ScenarioConfig,
) -> Result<BenchmarkResult, Box<dyn std::error::Error>> {
    let total_requests = scenario.target_qps * (scenario.duration_secs as usize);
    let interval = Duration::from_secs_f64(1.0 / (scenario.target_qps as f64));
    let zipf = if scenario.distribution == KeyDistribution::Zipfian {
        Some(ZipfGenerator::new(scenario.key_cardinality, 1.1))
    } else {
        None
    };

    let concurrency_limit = Arc::new(Semaphore::new(4096));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, Result<i32, String>)>(total_requests);

    let start_time = Instant::now();
    let mut next_scheduled = start_time;

    let dispatch_handle = tokio::spawn({
        let channel = channel.clone();
        let domain = scenario.domain.clone();
        let rule_count = scenario.rule_count;
        let hit_cost = scenario.hit_cost;
        let card = scenario.key_cardinality;
        let sem = concurrency_limit.clone();

        async move {
            let mut rng = FastRng::new(42);

            for req_idx in 0..total_requests {
                let now = Instant::now();
                if now < next_scheduled {
                    tokio::time::sleep(next_scheduled - now).await;
                }

                let scheduled_dispatch = next_scheduled;
                next_scheduled += interval;

                let key_id = match &zipf {
                    Some(z) => z.sample(&mut rng),
                    None => rng.gen_range(card),
                };

                let request = build_rate_limit_request(&domain, rule_count, key_id, hit_cost);
                let permit = match sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        // Queue saturated; drop with load error to protect benchmark process
                        let _ = tx
                            .send((100_000, Err("client_concurrency_saturated".to_string())))
                            .await;
                        continue;
                    }
                };

                let mut client = RateLimitServiceClient::new(channel.clone());
                let tx_clone = tx.clone();

                tokio::spawn(async move {
                    let _permit = permit;
                    let resp = client.should_rate_limit(tonic::Request::new(request)).await;
                    let latency = scheduled_dispatch.elapsed().as_micros() as u64;

                    let outcome = match resp {
                        Ok(r) => Ok(r.into_inner().overall_code),
                        Err(e) => Err(e.to_string()),
                    };

                    let _ = tx_clone.send((latency, outcome)).await;
                });

                if (req_idx + 1) % 10_000 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }
    });

    let mut tracker = LatencyTracker::with_capacity(total_requests);
    while tracker.samples.len() < total_requests {
        if let Some((latency, code)) = rx.recv().await {
            tracker.record(latency, code);
        } else {
            break;
        }
    }

    let _ = dispatch_handle.await;
    let elapsed = start_time.elapsed();

    let service_rss = read_rss_mb(std::process::id());
    Ok(tracker.summarize(
        &scenario.name,
        scenario.target_qps,
        elapsed,
        service_rss,
        0.0,
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("================================================================================");
    println!(" Steward Open-Loop Load Testing and Performance Certification Harness (M2.7)");
    println!("================================================================================");

    let redis_port = 16379;
    println!("1. Spawning dedicated ephemeral Redis on 127.0.0.1:{redis_port}...");
    let redis = DedicatedRedis::start(redis_port)?;
    let redis_pid = redis.pid();
    println!("   Redis running (PID {redis_pid})");

    let client = redis::Client::open(format!("redis://127.0.0.1:{redis_port}"))?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let _: () = redis::cmd("FLUSHALL").query_async(&mut conn).await?;

    println!("2. Compiling benchmark policy configurations...");
    let raw_config = generate_bench_config();
    let compiled_config = compile_rate_limits(raw_config)?;
    println!(
        "   Compiled {} domains, version hash: {}",
        compiled_config.len(),
        compiled_config.version_hash
    );

    let (_tx, rx) = tokio::sync::watch::channel(compiled_config);

    println!("3. Initializing Steward service on 127.0.0.1:50051...");
    let steward = Steward::try_new(
        &format!("127.0.0.1:{redis_port}"),
        10,
        rx,
        Arc::new(cadence::StatsdClient::from_sink("", cadence::NopMetricSink)),
    )
    .await?
    .with_execution_timeout(Duration::from_millis(20))
    .with_max_concurrent_requests(4096);

    let server_handle = tokio::spawn(async move {
        Server::builder()
            .concurrency_limit_per_connection(4096)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .tcp_nodelay(true)
            .add_service(RateLimitServiceServer::new(steward))
            .serve("127.0.0.1:50051".parse().unwrap())
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("4. Connecting benchmark client to gRPC server...");
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .concurrency_limit(4096)
        .tcp_nodelay(true)
        .connect()
        .await?;

    println!("5. Running Warmup Phase (2,000 QPS, 2s)...");
    let warmup_scenario = ScenarioConfig {
        name: "Warmup".to_string(),
        domain: "bench_fixed_window_1".to_string(),
        algorithm: "fixed_window".to_string(),
        rule_count: 1,
        hit_cost: 1,
        distribution: KeyDistribution::Uniform,
        target_qps: 2000,
        duration_secs: 2,
        key_cardinality: 1000,
    };
    let _ = run_grpc_benchmark(channel.clone(), warmup_scenario).await?;
    println!("   Warmup complete.");

    let mut results = Vec::new();

    // Matrix 1: Throughput and Saturation Curve (Fixed Window, 1 Rule, Cost 1, Uniform)
    println!("\n--- Matrix 1: Offered Throughput vs Saturation (Fixed Window, 1 Rule) ---");
    let target_rates = [5_000, 10_000, 15_000, 20_000, 25_000, 30_000];
    for qps in target_rates {
        println!("   Running offered load: {qps} QPS...");
        let sc = ScenarioConfig {
            name: format!("Throughput-{qps}-QPS"),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: qps,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us | Err: {}",
            res.completed_qps,
            res.p50_micros,
            res.p95_micros,
            res.p99_micros,
            res.p99_9_micros,
            res.error_count
        );
        results.push(res);
    }

    // Matrix 2: Rule Count Scaling (Fixed Window, 20k QPS)
    println!("\n--- Matrix 2: Rule Count Scaling (1, 2, 16 Rules at 20k QPS) ---");
    for rules in [1, 2, 16] {
        println!("   Running {rules}-rule match...");
        let sc = ScenarioConfig {
            name: format!("Rules-{rules}"),
            domain: format!("bench_fixed_window_{rules}"),
            algorithm: "fixed_window".to_string(),
            rule_count: rules,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us",
            res.completed_qps, res.p50_micros, res.p95_micros, res.p99_micros, res.p99_9_micros
        );
        results.push(res);
    }

    // Matrix 3: Algorithm Variation (1 Rule, 20k QPS)
    println!(
        "\n--- Matrix 3: Algorithm Variation (Fixed Window vs Token Bucket vs Sliding Window) ---"
    );
    for algo in ["fixed_window", "token_bucket", "sliding_window"] {
        println!("   Running algorithm: {algo}...");
        let sc = ScenarioConfig {
            name: format!("Algo-{algo}"),
            domain: format!("bench_{algo}_1"),
            algorithm: algo.to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us",
            res.completed_qps, res.p50_micros, res.p95_micros, res.p99_micros, res.p99_9_micros
        );
        results.push(res);
    }

    // Matrix 4: Key Distribution (Uniform vs Zipfian Skewed, 20k QPS)
    println!("\n--- Matrix 4: Key Distribution (Uniform vs Zipfian Skewed) ---");
    for dist in [KeyDistribution::Uniform, KeyDistribution::Zipfian] {
        let name = match dist {
            KeyDistribution::Uniform => "Uniform-Flat",
            KeyDistribution::Zipfian => "Zipfian-Skewed",
        };
        println!("   Running distribution: {name}...");
        let sc = ScenarioConfig {
            name: format!("Dist-{name}"),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: dist,
            target_qps: 20_000,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us",
            res.completed_qps, res.p50_micros, res.p95_micros, res.p99_micros, res.p99_9_micros
        );
        results.push(res);
    }

    // Matrix 5: Hit Cost Variation (1, 10, 100 hits per call at 20k QPS)
    println!("\n--- Matrix 5: Hit Cost Variation (1, 10, 100 hits) ---");
    for cost in [1, 10, 100] {
        println!("   Running hit cost: {cost} hits/call...");
        let sc = ScenarioConfig {
            name: format!("HitCost-{cost}"),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: cost,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us",
            res.completed_qps, res.p50_micros, res.p95_micros, res.p99_micros, res.p99_9_micros
        );
        results.push(res);
    }

    // Matrix 6: Over-Limit Rejection Latency
    println!("\n--- Matrix 6: Over-Limit Rejection Latency ---");
    {
        let sc = ScenarioConfig {
            name: "OverLimit-Rejection".to_string(),
            domain: "bench_overlimit".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: 3,
            key_cardinality: 1,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | Denied: {} | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us",
            res.completed_qps,
            res.denied_count,
            res.p50_micros,
            res.p95_micros,
            res.p99_micros,
            res.p99_9_micros
        );
        results.push(res);
    }

    // Matrix 7: High-Cardinality Churn (100,000 unique client keys at 20k QPS)
    println!("\n--- Matrix 7: High-Cardinality Churn (100,000 unique keys at 20k QPS) ---");
    {
        let sc = ScenarioConfig {
            name: "High-Cardinality-100k".to_string(),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: 5,
            key_cardinality: 100_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us | Service RSS: {:.1} MiB | Redis RSS: {:.1} MiB",
            res.completed_qps,
            res.p50_micros,
            res.p95_micros,
            res.p99_micros,
            res.p99_9_micros,
            res.service_rss_mb,
            res.redis_rss_mb
        );
        results.push(res);
    }

    // Matrix 8: 3x Traffic Burst (60,000 QPS Spike)
    println!("\n--- Matrix 8: 3x Traffic Burst (60,000 QPS Spike) ---");
    {
        let sc = ScenarioConfig {
            name: "Burst-3x-60000-QPS".to_string(),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 60_000,
            duration_secs: 3,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us | Service RSS: {:.1} MiB | Redis RSS: {:.1} MiB",
            res.completed_qps,
            res.p50_micros,
            res.p95_micros,
            res.p99_micros,
            res.p99_9_micros,
            res.service_rss_mb,
            res.redis_rss_mb
        );
        results.push(res);
    }

    // Matrix 9: Extended Steady-State Soak Run
    let soak_secs = if let Some(pos) = std::env::args().position(|a| a == "--soak-secs") {
        std::env::args()
            .nth(pos + 1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(21600)
    } else if std::env::args().any(|a| a == "--soak") {
        21600
    } else {
        10
    };

    println!("\n--- Matrix 9: Steady-State Soak (20,000 QPS, {soak_secs}s) ---");
    {
        let sc = ScenarioConfig {
            name: format!("Soak-20kQPS-{soak_secs}s"),
            domain: "bench_fixed_window_1".to_string(),
            algorithm: "fixed_window".to_string(),
            rule_count: 1,
            hit_cost: 1,
            distribution: KeyDistribution::Uniform,
            target_qps: 20_000,
            duration_secs: soak_secs,
            key_cardinality: 10_000,
        };
        let mut res = run_grpc_benchmark(channel.clone(), sc).await?;
        res.redis_rss_mb = read_rss_mb(redis_pid);
        println!(
            "     -> Completed: {:.0} QPS | p50: {}us | p95: {}us | p99: {}us | p99.9: {}us | Service RSS: {:.1} MiB | Redis RSS: {:.1} MiB",
            res.completed_qps,
            res.p50_micros,
            res.p95_micros,
            res.p99_micros,
            res.p99_9_micros,
            res.service_rss_mb,
            res.redis_rss_mb
        );
        results.push(res);
    }

    // Print summary table in GitHub markdown
    println!("\n================================================================================");
    println!(" BENCHMARK RESULTS SUMMARY (Markdown)");
    println!("================================================================================\n");

    println!(
        "| Scenario | Offered QPS | Completed QPS | Allowed | Denied | p50 (ms) | p95 (ms) | p99 (ms) | p99.9 (ms) | Service RSS | Redis RSS |"
    );
    println!("| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |");
    for r in &results {
        println!(
            "| {} | {:.0} | {:.0} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} MiB | {:.1} MiB |",
            r.scenario,
            r.offered_qps,
            r.completed_qps,
            r.allowed_count,
            r.denied_count,
            (r.p50_micros as f64) / 1000.0,
            (r.p95_micros as f64) / 1000.0,
            (r.p99_micros as f64) / 1000.0,
            (r.p99_9_micros as f64) / 1000.0,
            r.service_rss_mb,
            r.redis_rss_mb,
        );
    }

    println!("\nBenchmark run completed successfully.");
    server_handle.abort();
    Ok(())
}
