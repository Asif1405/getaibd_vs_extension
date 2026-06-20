use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Round-trip gate that delegates command execution to the client (a managed terminal)
/// and waits for the captured result (a JSON string with stdout/stderr/exit_code).
#[derive(Clone)]
pub struct TerminalGate {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    timeout_secs: u64,
}

impl TerminalGate {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: 1800,
        }
    }

    /// Registers a request and blocks until the client posts its result or the wait times out.
    pub async fn request(&self, request_id: String) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), rx).await {
            Ok(Ok(result)) => Some(result),
            _ => {
                self.pending.lock().await.remove(&request_id);
                None
            }
        }
    }

    /// Delivers the client's captured result back to the waiting request.
    pub async fn respond(&self, request_id: &str, result: String) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(request_id) {
            tx.send(result).is_ok()
        } else {
            false
        }
    }
}

impl Default for TerminalGate {
    fn default() -> Self {
        Self::new()
    }
}
