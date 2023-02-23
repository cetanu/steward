use redis::{Client, Commands, Connection, RedisResult, Value};

const INTERVAL: usize = 10;

pub fn connect(addr: &str) -> RedisResult<Connection> {
    let client = Client::open(format!("redis://{addr}/"))?;
    client.get_connection()
}

pub fn increment_entry(con: &mut Connection, domain: &str, key: &str, hits: &u32) -> i64 {
    let existed = con.exists(key);
    let incremented_value = con.hincr(domain, key, hits);

    let value = match incremented_value {
        Ok(Value::Int(n)) => n,
        _ => panic!(),
    };
    if let Ok(false) = existed {
        con.expire::<&str, u64>(key, INTERVAL).unwrap();
    }
    value
}
