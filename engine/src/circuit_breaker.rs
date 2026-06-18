use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Circuit breaker state for a specific resource (provider/tool)
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitState {
    /// Circuit is closed, requests flow normally
    Closed,
    /// Circuit is open, requests are blocked
    Open { opened_at: Instant },
    /// Circuit is half-open, testing if resource recovered
    HalfOpen,
}

/// Statistics for a circuit
#[derive(Debug, Clone)]
struct CircuitStats {
    failures: u32,
    successes: u32,
    last_failure: Option<Instant>,
    state: CircuitState,
}

impl Default for CircuitStats {
    fn default() -> Self {
        Self {
            failures: 0,
            successes: 0,
            last_failure: None,
            state: CircuitState::Closed,
        }
    }
}

/// Configuration for circuit breaker
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of failures in window before opening circuit
    pub failure_threshold: u32,
    /// Time window for counting failures
    pub failure_window: Duration,
    /// How long to wait before attempting recovery
    pub recovery_timeout: Duration,
    /// Number of successes needed in half-open state to close
    pub success_threshold: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            failure_window: Duration::from_secs(60),
            recovery_timeout: Duration::from_secs(300), // 5 minutes
            success_threshold: 2,
        }
    }
}

/// Circuit breaker for tracking provider/tool failures
pub struct CircuitBreaker {
    circuits: Arc<RwLock<HashMap<String, CircuitStats>>>,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            circuits: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Check if a circuit is open (request should be blocked)
    pub async fn is_open(&self, resource_id: &str) -> bool {
        let circuits = self.circuits.read().await;
        if let Some(stats) = circuits.get(resource_id) {
            match &stats.state {
                CircuitState::Open { opened_at } => {
                    // Check if recovery timeout elapsed
                    if opened_at.elapsed() >= self.config.recovery_timeout {
                        // Should transition to half-open
                        false
                    } else {
                        true
                    }
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Get current state of a circuit
    pub async fn get_state(&self, resource_id: &str) -> CircuitState {
        let mut circuits = self.circuits.write().await;
        let stats = circuits.entry(resource_id.to_string()).or_default();

        match &stats.state {
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.config.recovery_timeout {
                    // Transition to half-open
                    stats.state = CircuitState::HalfOpen;
                    stats.successes = 0;
                    tracing::info!(
                        resource = resource_id,
                        "Circuit breaker transitioning to half-open"
                    );
                }
            }
            _ => {}
        }

        stats.state.clone()
    }

    /// Record a successful operation
    pub async fn record_success(&self, resource_id: &str) {
        let mut circuits = self.circuits.write().await;
        let stats = circuits.entry(resource_id.to_string()).or_default();

        stats.successes += 1;

        match &stats.state {
            CircuitState::HalfOpen => {
                if stats.successes >= self.config.success_threshold {
                    // Transition back to closed
                    stats.state = CircuitState::Closed;
                    stats.failures = 0;
                    tracing::info!(
                        resource = resource_id,
                        "Circuit breaker closed after successful recovery"
                    );
                }
            }
            CircuitState::Closed => {
                // Reset failure count on success
                if stats.failures > 0 {
                    stats.failures = 0;
                }
            }
            _ => {}
        }
    }

    /// Record a failed operation
    pub async fn record_failure(&self, resource_id: &str) {
        let mut circuits = self.circuits.write().await;
        let stats = circuits.entry(resource_id.to_string()).or_default();

        let now = Instant::now();

        // Clean old failures outside window
        if let Some(last_failure) = stats.last_failure {
            if now.duration_since(last_failure) > self.config.failure_window {
                stats.failures = 0;
            }
        }

        stats.failures += 1;
        stats.last_failure = Some(now);

        match &stats.state {
            CircuitState::Closed => {
                if stats.failures >= self.config.failure_threshold {
                    // Open the circuit
                    stats.state = CircuitState::Open { opened_at: now };
                    tracing::warn!(
                        resource = resource_id,
                        failures = stats.failures,
                        "Circuit breaker opened due to repeated failures"
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Failure in half-open immediately reopens
                stats.state = CircuitState::Open { opened_at: now };
                stats.successes = 0;
                tracing::warn!(
                    resource = resource_id,
                    "Circuit breaker reopened after failure in half-open state"
                );
            }
            _ => {}
        }
    }

    /// Get failure count for a resource
    pub async fn get_failure_count(&self, resource_id: &str) -> u32 {
        let circuits = self.circuits.read().await;
        circuits.get(resource_id).map(|s| s.failures).unwrap_or(0)
    }

    /// Reset a circuit (for testing/admin purposes)
    pub async fn reset(&self, resource_id: &str) {
        let mut circuits = self.circuits.write().await;
        circuits.remove(resource_id);
        tracing::info!(resource = resource_id, "Circuit breaker reset");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_failures() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            failure_window: Duration::from_secs(60),
            recovery_timeout: Duration::from_secs(5),
            success_threshold: 2,
        };
        let breaker = CircuitBreaker::new(config);

        // Record 3 failures
        for _ in 0..3 {
            breaker.record_failure("test-provider").await;
        }

        // Should be open
        assert!(breaker.is_open("test-provider").await);
    }

    #[tokio::test]
    async fn test_circuit_breaker_recovers() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            failure_window: Duration::from_secs(60),
            recovery_timeout: Duration::from_millis(100),
            success_threshold: 2,
        };
        let breaker = CircuitBreaker::new(config);

        // Trigger open state
        breaker.record_failure("test-provider").await;
        breaker.record_failure("test-provider").await;
        assert!(breaker.is_open("test-provider").await);

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should transition to half-open
        let state = breaker.get_state("test-provider").await;
        assert_eq!(state, CircuitState::HalfOpen);

        // Record successes to close
        breaker.record_success("test-provider").await;
        breaker.record_success("test-provider").await;

        let state = breaker.get_state("test-provider").await;
        assert_eq!(state, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_reopens_on_half_open_failure() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            failure_window: Duration::from_secs(60),
            recovery_timeout: Duration::from_millis(100),
            success_threshold: 2,
        };
        let breaker = CircuitBreaker::new(config);

        // Open circuit
        breaker.record_failure("test-provider").await;
        breaker.record_failure("test-provider").await;

        // Wait for half-open
        tokio::time::sleep(Duration::from_millis(150)).await;
        breaker.get_state("test-provider").await;

        // Fail in half-open state
        breaker.record_failure("test-provider").await;

        // Should be open again
        assert!(breaker.is_open("test-provider").await);
    }
}
