use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

#[derive(Clone)]
pub struct ApprovalGate {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout_secs: u64,
}

impl ApprovalGate {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: 300, // 5 minutes default
        }
    }

    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs,
        }
    }

    pub async fn request(&self, request_id: String) -> bool {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => {
                // Channel closed without response
                self.pending.lock().await.remove(&request_id);
                false
            }
            Err(_) => {
                // Timeout expired
                self.pending.lock().await.remove(&request_id);
                false
            }
        }
    }

    pub async fn respond(&self, request_id: &str, approved: bool) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(request_id) {
            tx.send(approved).is_ok()
        } else {
            false
        }
    }
}

impl Default for ApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}
