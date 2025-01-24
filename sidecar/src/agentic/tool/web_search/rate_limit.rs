use lazy_static::lazy_static;
use std::sync::Mutex;

const RATE_LIMIT_PER_SECOND: u32 = 5;

pub fn check_rate_limit() -> Result<(), anyhow::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let mut count = REQUEST_COUNT.lock().map_err(|_| anyhow::anyhow!("Failed to acquire lock"))?;

    if now - count.last_reset > 1000 {
        count.second = 0;
        count.last_reset = now;
    }

    if count.second >= RATE_LIMIT_PER_SECOND {
        return Err(anyhow::anyhow!("Rate limit exceeded"));
    }

    count.second += 1;

    Ok(())
}

#[derive(Debug)]
struct RequestCount {
    second: u32,
    last_reset: u128,
}

impl RequestCount {
    fn new() -> Self {
        Self {
            second: 0,
            last_reset: 0,
        }
    }
}

lazy_static! {
    static ref REQUEST_COUNT: Mutex<RequestCount> = Mutex::new(RequestCount::new());
}