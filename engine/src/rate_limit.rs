use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
}

pub struct RateLimiter {
    limits: HashMap<String, RateLimitConfig>,
    windows: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(limits: HashMap<String, RateLimitConfig>) -> Arc<Self> {
        Arc::new(Self {
            limits,
            windows: Mutex::new(HashMap::new()),
        })
    }

    pub async fn check(&self, provider: &str) -> Result<(), u64> {
        let Some(limit) = self.limits.get(provider) else {
            return Ok(());
        };

        let max_rpm = limit.requests_per_minute;
        if max_rpm == 0 {
            return Ok(());
        }

        let now = Instant::now();
        let window = std::time::Duration::from_secs(60);
        let cutoff = now.checked_sub(window).unwrap_or(now);

        let mut windows = self.windows.lock().await;
        let entries = windows.entry(provider.to_string()).or_default();
        entries.retain(|t| *t > cutoff);

        if entries.len() >= max_rpm as usize {
            let oldest = entries[0];
            let retry_after = (oldest + window).duration_since(now).as_secs() + 1;
            return Err(retry_after);
        }

        entries.push(now);
        Ok(())
    }
}
