use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Round-trip gate that delegates a clarifying question to the client (an
/// interactive options box) and waits for the user's selected answer (a plain
/// string). Mirrors `TerminalGate` but is kept separate so questions and
/// terminal results never cross wires.
#[derive(Clone)]
pub struct AskGate {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    timeout_secs: u64,
}

impl AskGate {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: 1800,
        }
    }

    /// Registers a question and blocks until the client posts the answer or it times out.
    pub async fn request(&self, request_id: String) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), rx).await {
            Ok(Ok(answer)) => Some(answer),
            _ => {
                self.pending.lock().await.remove(&request_id);
                None
            }
        }
    }

    /// Delivers the user's answer back to the waiting question.
    pub async fn respond(&self, request_id: &str, answer: String) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(request_id) {
            tx.send(answer).is_ok()
        } else {
            false
        }
    }
}

impl Default for AskGate {
    fn default() -> Self {
        Self::new()
    }
}
