use std::path::PathBuf;
use uuid::Uuid;

use crate::models::ToolMessage;

pub struct Session {
    pub id: String,
    pub provider_id: String,
    pub model: String,
    pub project_root: PathBuf,
    pub messages: Vec<ToolMessage>,
    pub max_iterations: u32,
    pub system_prompt: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl Session {
    pub fn new(
        provider_id: impl Into<String>,
        model: impl Into<String>,
        project_root: PathBuf,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            provider_id: provider_id.into(),
            model: model.into(),
            project_root,
            messages: Vec::new(),
            max_iterations: 25,
            system_prompt: None,
            reasoning_effort: None,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn push_message(&mut self, msg: ToolMessage) {
        self.messages.push(msg);
    }

    /// Compress older messages into a summary, keeping the most recent half.
    pub fn compress_messages(&mut self) {
        if self.messages.len() <= 4 {
            return;
        }
        let keep_count = self.messages.len() / 2;
        let to_summarize = self.messages.len() - keep_count;

        let old: Vec<ToolMessage> = self.messages.drain(..to_summarize).collect();
        let summary = old
            .iter()
            .filter_map(|m| {
                let content = m.content.as_deref().unwrap_or("");
                if content.is_empty() {
                    None
                } else {
                    Some(format!("[{}] {}", m.role, truncate_str(content, 200)))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !summary.is_empty() {
            self.messages.insert(
                0,
                ToolMessage::system(format!("Previous conversation summary:\n{summary}")),
            );
        }
    }
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}
