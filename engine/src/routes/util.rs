//! Shared helpers for the streaming agent routes: tying a spawned agent task's
//! lifetime to the SSE response, and guaranteeing session gates are cleared no
//! matter how the task ends (normal return, panic, or abort on disconnect).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;

use crate::state::AppState;

/// Aborts a spawned task when dropped. Attached to the SSE stream so that when a
/// client disconnects (the response body is dropped by axum), the background
/// agent run is aborted instead of continuing to burn CPU, LLM tokens, and MCP
/// subprocesses for nobody. Aborting a task that already finished is a no-op.
pub struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl AbortOnDrop {
    pub fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(handle)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Wraps an SSE event stream and holds an [`AbortOnDrop`] guard so the producing
/// task is cancelled when the stream is dropped. Delegates all polling to the
/// inner stream.
pub struct GuardedStream<S> {
    inner: S,
    _guard: AbortOnDrop,
}

impl<S> GuardedStream<S> {
    pub fn new(inner: S, handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            inner,
            _guard: AbortOnDrop::new(handle),
        }
    }
}

impl<S: Stream + Unpin> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Clears every session-scoped gate on drop. Living inside the spawned agent
/// task, this runs whether the task returns normally, panics, or is aborted
/// (dropped) because the client disconnected — so orphaned gate entries can no
/// longer accumulate in `AppState`. Clearing a gate that was never set is a
/// cheap no-op.
pub struct GateCleanup {
    state: Arc<AppState>,
    session_id: String,
}

impl GateCleanup {
    pub fn new(state: Arc<AppState>, session_id: String) -> Self {
        Self { state, session_id }
    }
}

impl Drop for GateCleanup {
    fn drop(&mut self) {
        self.state.clear_approval_gate(&self.session_id);
        self.state.clear_ask_gate(&self.session_id);
        self.state.clear_terminal_gate(&self.session_id);
        self.state.clear_editor_gate(&self.session_id);
    }
}
