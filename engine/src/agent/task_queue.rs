use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub description: String,
    pub status: TaskStatus,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub progress: Vec<String>,
}

impl TaskRecord {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            description: description.into(),
            status: TaskStatus::Queued,
            created_at: unix_now(),
            started_at: None,
            finished_at: None,
            output: None,
            error: None,
            progress: vec![],
        }
    }
}

pub type TaskStore = Arc<RwLock<HashMap<String, TaskRecord>>>;

/// Hard cap on retained task records. Beyond this, the oldest *finished*
/// (completed/failed/cancelled) records are evicted so a long-lived server does
/// not accumulate task history for its whole lifetime. Active (queued/running)
/// tasks are never evicted.
const MAX_RETAINED_TASKS: usize = 200;

/// Cap on the per-task progress log. A single long-running task could otherwise
/// push unbounded progress lines into memory. We keep the most recent lines.
const MAX_PROGRESS_LINES: usize = 500;

pub struct TaskQueue {
    store: TaskStore,
    tx: mpsc::Sender<QueuedTask>,
}

struct QueuedTask {
    id: String,
    work: Box<dyn FnOnce(TaskHandle) -> futures::future::BoxFuture<'static, ()> + Send>,
}

#[derive(Clone)]
pub struct TaskHandle {
    id: String,
    store: TaskStore,
}

impl TaskHandle {
    pub async fn progress(&self, msg: impl Into<String>) {
        let mut store = self.store.write().await;
        if let Some(record) = store.get_mut(&self.id) {
            record.progress.push(msg.into());
            // Keep only the most recent lines so a chatty task can't grow without bound.
            let len = record.progress.len();
            if len > MAX_PROGRESS_LINES {
                record.progress.drain(0..len - MAX_PROGRESS_LINES);
            }
        }
    }

    pub async fn complete(&self, output: impl Into<String>) {
        let mut store = self.store.write().await;
        if let Some(record) = store.get_mut(&self.id) {
            record.status = TaskStatus::Completed;
            record.output = Some(output.into());
            record.finished_at = Some(unix_now());
        }
    }

    pub async fn fail(&self, error: impl Into<String>) {
        let mut store = self.store.write().await;
        if let Some(record) = store.get_mut(&self.id) {
            record.status = TaskStatus::Failed;
            record.error = Some(error.into());
            record.finished_at = Some(unix_now());
        }
    }
}

impl TaskQueue {
    pub fn new() -> Self {
        let store: TaskStore = Arc::new(RwLock::new(HashMap::new()));
        let (tx, rx) = mpsc::channel::<QueuedTask>(64);
        let store_clone = store.clone();

        // Only spawn the worker if we're inside a tokio runtime.
        // In synchronous test contexts there is no runtime, so we skip the
        // spawn — the queue will simply not process tasks (acceptable for unit
        // tests that only test state construction).
        if tokio::runtime::Handle::try_current().is_ok() {
            let mut rx = rx;
            tokio::spawn(async move {
                while let Some(task) = rx.recv().await {
                    let handle = TaskHandle {
                        id: task.id.clone(),
                        store: store_clone.clone(),
                    };

                    {
                        let mut s = store_clone.write().await;
                        if let Some(record) = s.get_mut(&task.id) {
                            record.status = TaskStatus::Running;
                            record.started_at = Some(unix_now());
                        }
                    }

                    (task.work)(handle).await;
                }
            });
        } else {
            // Drop rx so the channel is closed; enqueue will still record tasks
            // but they won't be executed (test-only scenario).
            drop(rx);
            return Self { store, tx };
        }

        Self { store, tx }
    }

    pub async fn enqueue<F, Fut>(&self, description: impl Into<String>, work: F) -> String
    where
        F: FnOnce(TaskHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let record = TaskRecord::new(description);
        let id = record.id.clone();

        {
            let mut store = self.store.write().await;
            store.insert(id.clone(), record);
            evict_finished(&mut store, MAX_RETAINED_TASKS);
        }

        let _ = self
            .tx
            .send(QueuedTask {
                id: id.clone(),
                work: Box::new(move |h| Box::pin(work(h))),
            })
            .await;

        id
    }

    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.store.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<TaskRecord> {
        let store = self.store.read().await;
        let mut records: Vec<TaskRecord> = store.values().cloned().collect();
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        records
    }

    pub async fn cancel(&self, id: &str) -> bool {
        let mut store = self.store.write().await;
        if let Some(record) = store.get_mut(id) {
            if record.status == TaskStatus::Queued {
                record.status = TaskStatus::Cancelled;
                record.finished_at = Some(unix_now());
                return true;
            }
        }
        false
    }
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Evict oldest *finished* task records once the store exceeds `max`. Queued and
/// running tasks are always kept; only terminal records (completed/failed/
/// cancelled) are candidates for removal, oldest-first by creation time.
fn evict_finished(store: &mut HashMap<String, TaskRecord>, max: usize) {
    if store.len() <= max {
        return;
    }
    let mut finished: Vec<(String, u64)> = store
        .values()
        .filter(|r| {
            matches!(
                r.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        })
        .map(|r| (r.id.clone(), r.created_at))
        .collect();
    // Oldest first.
    finished.sort_by(|a, b| a.1.cmp(&b.1));

    let mut to_remove = store.len().saturating_sub(max);
    for (id, _) in finished {
        if to_remove == 0 {
            break;
        }
        store.remove(&id);
        to_remove -= 1;
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
