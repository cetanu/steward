use redis::{Client, Commands, Connection, RedisResult, Value};

impl RedisClient {
    pub fn new(addr: &str, interval: usize) -> Self {
        Self {
            con: Self::connect(addr).unwrap(),
            interval,
        }
    }

    fn connect(addr: &str) -> RedisResult<Connection> {
        let client = Client::open(format!("redis://{addr}/"))?;
        client.get_connection()
    }

    pub fn increment_entry(&mut self, key: &str, hits: &u32, interval: Option<usize>) -> i64 {
        let existed = self.con.exists(key);
        let incremented_value = self.con.incr(key, hits);

        let value = match incremented_value {
            Ok(Value::Int(n)) => n,
            _ => panic!(),
        };
        if let Ok(false) = existed {
            self.con
                .expire::<&str, u64>(key, interval.unwrap_or(self.interval))
                .unwrap();
        }
        value
    }
}

pub struct RedisClient {
    con: Connection,
    interval: usize,
}
