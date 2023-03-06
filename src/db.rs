use redis::{Client, Commands, Connection, RedisResult, Value};

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
        let existed = self.db.exists(key);
        let incremented_value = self.db.incr(key, hits);
        if let Ok(Value::Int(n)) = incremented_value {
            current_rate = n;
        }
        if let Ok(false) = existed {
            self.db
                .expire::<&str, u64>(key, interval.unwrap_or(self.interval))
                .unwrap();
        }
        current_rate
    }
}

pub struct RedisClient {
    db: Connection,
    interval: usize,
}
