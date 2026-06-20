use serde::Deserialize;

use crate::models::{Message, ToolMessage};

/// Approximate token count from text. Most LLM tokenizers average ~4 chars per token.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

fn message_tokens(msg: &Message) -> usize {
    estimate_tokens(&msg.content) + 4 // role overhead
}

/// Known context window sizes per model prefix.
fn model_context_limit(model: &str) -> usize {
    let m = model.to_lowercase();
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        128_000
    } else if m.contains("gpt-4") {
        8_192
    } else if m.contains("gpt-3.5") {
        16_385
    } else if m.contains("claude-3") || m.contains("claude-sonnet") || m.contains("claude-opus") {
        200_000
    } else if m.contains("gemini-2") || m.contains("gemini-1.5-pro") {
        1_000_000
    } else if m.contains("gemini") {
        32_000
    } else if m.contains("grok") {
        131_072
    } else if m.contains("deepseek") {
        64_000
    } else if m.contains("llama-3.1-405b") || m.contains("llama-3.1-70b") {
        128_000
    } else {
        8_192
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrimStrategy {
    /// Drop oldest non-system messages first.
    DropOldest,
    /// Summarize dropped messages into a single system note (placeholder for future).
    Summarize,
}

impl Default for TrimStrategy {
    fn default() -> Self {
        Self::DropOldest
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub strategy: TrimStrategy,
    #[serde(default = "default_reserve_ratio")]
    pub reserve_for_completion: f32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: None,
            strategy: TrimStrategy::default(),
            reserve_for_completion: default_reserve_ratio(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_reserve_ratio() -> f32 {
    0.25
}

/// Trims messages to fit within the context window for the given model.
///
/// Preserves:
/// - All system messages (moved to front)
/// - The most recent user message (always kept)
/// - As many recent messages as fit
///
/// Returns trimmed messages and a flag indicating if trimming occurred.
pub fn trim_to_context(
    messages: &[Message],
    model: &str,
    config: &ContextConfig,
) -> (Vec<Message>, bool) {
    if !config.enabled || messages.is_empty() {
        return (messages.to_vec(), false);
    }

    let ctx_limit = config.max_tokens.unwrap_or_else(|| model_context_limit(model));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let budget = (ctx_limit as f32 * (1.0 - config.reserve_for_completion)) as usize;

    let total: usize = messages.iter().map(message_tokens).sum();
    if total <= budget {
        return (messages.to_vec(), false);
    }

    let mut system_msgs: Vec<&Message> = Vec::new();
    let mut non_system: Vec<&Message> = Vec::new();

    for msg in messages {
        if msg.role == "system" {
            system_msgs.push(msg);
        } else {
            non_system.push(msg);
        }
    }

    let system_cost: usize = system_msgs.iter().map(|m| message_tokens(m)).sum();
    let remaining_budget = budget.saturating_sub(system_cost);

    // Walk non-system messages from most recent backwards, keeping what fits
    let mut kept: Vec<&Message> = Vec::new();
    let mut used = 0;
    for msg in non_system.iter().rev() {
        let cost = message_tokens(msg);
        if used + cost > remaining_budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(msg);
    }
    kept.reverse();

    let trimmed = non_system.len() != kept.len();

    let mut result: Vec<Message> = system_msgs.into_iter().cloned().collect();
    if trimmed {
        let dropped_msgs: Vec<&Message> = non_system[..non_system.len() - kept.len()].to_vec();
        let dropped = dropped_msgs.len();

        match config.strategy {
            TrimStrategy::Summarize => {
                let summary = summarize_messages(&dropped_msgs);
                result.push(Message {
                    role: "system".to_string(),
                    content: format!("[{dropped} earlier message(s) summarized]\n{summary}"),
                });
            }
            TrimStrategy::DropOldest => {
                result.push(Message {
                    role: "system".to_string(),
                    content: format!("[{dropped} earlier message(s) trimmed to fit context window]"),
                });
            }
        }
    }
    result.extend(kept.into_iter().cloned());

    (result, trimmed)
}

fn summarize_messages(msgs: &[&Message]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        if msg.content.is_empty() {
            continue;
        }
        let role_label = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            _ => &msg.role,
        };
        let truncated = if msg.content.len() > 150 {
            format!("{}...", &msg.content[..150])
        } else {
            msg.content.clone()
        };
        lines.push(format!("- {role_label}: {truncated}"));
    }
    if lines.is_empty() {
        "No meaningful content in dropped messages.".to_string()
    } else {
        lines.join("\n")
    }
}

fn tool_message_tokens(msg: &ToolMessage) -> usize {
    let content_len = msg.content.as_deref().map_or(0, str::len);
    let tool_calls_len = msg
        .tool_calls
        .as_ref()
        .map_or(0, |calls| calls.iter().map(|c| c.name.len() + c.arguments.to_string().len()).sum());
    (content_len + tool_calls_len) / 4 + 4
}

/// Trim `ToolMessage` list (used by the agent loop).
/// Preserves system messages and the most recent messages that fit.
pub fn trim_to_context_tool_messages(
    messages: &[ToolMessage],
    model: &str,
    config: &ContextConfig,
) -> (Vec<ToolMessage>, bool) {
    if !config.enabled || messages.is_empty() {
        return (messages.to_vec(), false);
    }

    let ctx_limit = config.max_tokens.unwrap_or_else(|| model_context_limit(model));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let budget = (ctx_limit as f32 * (1.0 - config.reserve_for_completion)) as usize;

    let total: usize = messages.iter().map(tool_message_tokens).sum();
    if total <= budget {
        return (messages.to_vec(), false);
    }

    let mut system_msgs: Vec<&ToolMessage> = Vec::new();
    let mut non_system: Vec<&ToolMessage> = Vec::new();

    for msg in messages {
        if msg.role == "system" {
            system_msgs.push(msg);
        } else {
            non_system.push(msg);
        }
    }

    let system_cost: usize = system_msgs.iter().map(|m| tool_message_tokens(m)).sum();
    let remaining_budget = budget.saturating_sub(system_cost);

    let mut kept: Vec<&ToolMessage> = Vec::new();
    let mut used = 0;
    for msg in non_system.iter().rev() {
        let cost = tool_message_tokens(msg);
        if used + cost > remaining_budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(msg);
    }
    kept.reverse();

    // Never start the kept window with an orphaned tool result whose preceding
    // assistant tool_call message was trimmed away — that corrupts the request.
    while kept.first().is_some_and(|m| m.role == "tool") {
        kept.remove(0);
    }

    let trimmed = non_system.len() != kept.len();

    let mut result: Vec<ToolMessage> = system_msgs.into_iter().cloned().collect();
    if trimmed {
        let dropped_msgs: Vec<&ToolMessage> = non_system[..non_system.len() - kept.len()].to_vec();
        let dropped = dropped_msgs.len();

        match config.strategy {
            TrimStrategy::Summarize => {
                let summary = summarize_tool_messages(&dropped_msgs);
                result.push(ToolMessage::system(format!(
                    "[{dropped} earlier message(s) summarized]\n{summary}"
                )));
            }
            TrimStrategy::DropOldest => {
                result.push(ToolMessage::system(format!(
                    "[{dropped} earlier message(s) trimmed to fit context window]"
                )));
            }
        }
    }
    result.extend(kept.into_iter().cloned());

    (result, trimmed)
}

fn summarize_tool_messages(msgs: &[&ToolMessage]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        let content = msg.content.as_deref().unwrap_or("");
        if content.is_empty() && msg.tool_calls.as_ref().is_none_or(|c| c.is_empty()) {
            continue;
        }
        let role_label = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            "tool" => "Tool result",
            _ => &msg.role,
        };
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                lines.push(format!("- {role_label} called tool `{}`", call.name));
            }
        } else if !content.is_empty() {
            let truncated = if content.len() > 150 {
                format!("{}...", &content[..150])
            } else {
                content.to_string()
            };
            lines.push(format!("- {role_label}: {truncated}"));
        }
    }
    if lines.is_empty() {
        "No meaningful content in dropped messages.".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn no_trim_when_under_budget() {
        let msgs = vec![msg("user", "hello")];
        let cfg = ContextConfig::default();
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(!trimmed);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn trims_oldest_non_system() {
        let mut msgs = vec![msg("system", "you are helpful")];
        for i in 0..500 {
            msgs.push(msg("user", &"x".repeat(200)));
            msgs.push(msg("assistant", &format!("response {i} with lots of text to fill the context window quickly")));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::DropOldest,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result.len() < msgs.len());
        assert_eq!(result[0].role, "system");
        assert_eq!(result[0].content, "you are helpful");
        // Second message should be the trim notice
        assert!(result[1].content.contains("trimmed"));
    }

    #[test]
    fn disabled_does_nothing() {
        let msgs = vec![msg("user", &"x".repeat(100_000))];
        let cfg = ContextConfig {
            enabled: false,
            ..Default::default()
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(!trimmed);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn summarize_strategy_includes_content() {
        let mut msgs = vec![msg("system", "you are helpful")];
        for i in 0..500 {
            msgs.push(msg("user", &format!("question {i}")));
            msgs.push(msg("assistant", &"a".repeat(200)));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::Summarize,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result[1].content.contains("summarized"));
        assert!(result[1].content.contains("User:"));
    }

    fn tool_msg(role: &str, content: &str) -> ToolMessage {
        ToolMessage {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn tool_message_trim_preserves_system() {
        let mut msgs = vec![tool_msg("system", "system prompt")];
        for _ in 0..500 {
            msgs.push(tool_msg("user", &"long user message".repeat(20)));
            msgs.push(tool_msg("assistant", "response"));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::DropOldest,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context_tool_messages(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result.len() < msgs.len());
        assert_eq!(result[0].role, "system");
    }

    #[test]
    fn tool_message_summarize_strategy() {
        let mut msgs = vec![tool_msg("system", "system prompt")];
        for _ in 0..500 {
            msgs.push(tool_msg("user", "tell me something"));
            msgs.push(tool_msg("assistant", "here you go"));
        }
        let cfg = ContextConfig {
            enabled: true,
            max_tokens: Some(1000),
            strategy: TrimStrategy::Summarize,
            reserve_for_completion: 0.25,
        };
        let (result, trimmed) = trim_to_context_tool_messages(&msgs, "gpt-4o", &cfg);
        assert!(trimmed);
        assert!(result[1].content.as_deref().unwrap_or("").contains("summarized"));
    }
}
