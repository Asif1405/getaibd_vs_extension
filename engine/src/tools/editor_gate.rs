use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Round-trip gate that delegates a structural query (LSP references / definition /
/// symbols) or a post-edit diagnostics request to the editor and waits for the
/// captured result (a JSON string). Mirrors `TerminalGate`/`AskGate` but is kept
/// separate so editor results never cross wires with terminal or question replies.
#[derive(Clone)]
pub struct EditorGate {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    timeout_secs: u64,
}

impl EditorGate {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            // LSP round-trips are quick; keep the wait short so a headless or
            // unresponsive editor never stalls the whole turn for long.
            timeout_secs: 20,
        }
    }

    /// Registers a request and blocks until the editor posts its result or it times out.
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

    /// Delivers the editor's result back to the waiting request.
    pub async fn respond(&self, request_id: &str, result: String) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(request_id) {
            tx.send(result).is_ok()
        } else {
            false
        }
    }
}

impl Default for EditorGate {
    fn default() -> Self {
        Self::new()
    }
}
