use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::tools::Tool;

/// Reads earlier terminal output from the current session's managed terminal pool.
///
/// Execution is delegated to the client (see `runtime::delegate_terminal`), which keeps
/// per-terminal scrollback for every command run this session. When no client terminal is
/// connected (headless engine run) this falls back to a note, since there is no pool to
/// read from.
pub struct ReadTerminal;

impl ReadTerminal {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReadTerminal {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadTerminal {
    fn name(&self) -> &'static str {
        "read_terminal"
    }

    fn description(&self) -> &'static str {
        "Read output from earlier terminal runs in this session. Commands run in a \
         persistent pool of terminals; each run_command result reports the terminal_id it \
         used. Call with no arguments to list every terminal and its recent runs, or pass a \
         terminal_id to get that terminal's full scrollback. Use this to inspect logs from a \
         dev server/watcher you started earlier, or to re-check output you didn't fully read."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "terminal_id": { "type": "string", "description": "Optional terminal to read (from a prior result's terminal_id). Omit to list all terminals and their recent runs." }
            }
        })
    }

    async fn execute(&self, _input: Value) -> Result<Value, AppError> {
        // Only reached when no client terminal pool is connected (headless run).
        Ok(json!({
            "error": "No terminal pool is connected in this run; terminal history is unavailable."
        }))
    }
}
