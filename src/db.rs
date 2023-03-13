use redis::{Client, Commands, Connection, RedisResult, Value};
use tracing::{debug, error};

impl RedisClient {
    pub fn new(addr: &str, interval: usize) -> Self {
        Self {
            db: Self::connect(addr).unwrap(),
            interval,
        }
    }

    fn connect(addr: &str) -> RedisResult<Connection> {
        let client = Client::open(format!("redis://{addr}/"))?;
        client.get_connection()
    }

    pub fn increment_entry(&mut self, key: &str, hits: &u32, interval: Option<usize>) -> i64 {
        let mut current_rate = 0;
        let interval = interval.unwrap_or(self.interval);
        let incremented_value = self.db.incr(key, hits);
        if let Ok(Value::Int(n)) = incremented_value {
            current_rate = n;
        }
        // Key was created, because 1 means that INCR was performed on either 0 or null key
        // Therefore, we should set it to expire
        if current_rate == 1 {
            match self.db.expire::<&str, u64>(key, interval) {
                Err(e) => error!("Failed to set expiry for key: {e}"),
                Ok(_) => debug!("Set expiry for key to {interval}"),
            }
        }
        current_rate
    }
}

pub struct RedisClient {
    db: Connection,
    interval: usize,
}
