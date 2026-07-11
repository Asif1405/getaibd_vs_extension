/// System prompts and response parsing for plan/think/reflect agent loop.
pub const PLANNING_PROMPT: &str = "\
Work like a developer who thinks before acting and checks progress after each move.

1. PLAN: Before acting, briefly break the task into ordered steps inside <plan>...</plan> tags —
   how you would actually approach it, not ceremony.
2. THINK: When reasoning through a problem, wrap your thoughts in <thinking>...</thinking> tags.
3. ACT: Execute the next step with the available tools. Pick the cheapest tool that does the job
   and don't repeat a call that already gave its answer.
4. REFLECT: After tool results come back, inside <reflection>...</reflection> tags ask 'did this
   move me toward done?' — if yes, continue; if it failed or the result was unexpected, adapt and
   output a revised <plan> instead of retrying the same thing.

Done when the task's goal is met and verified. Stop then — do not keep planning or exploring past
that point.";

pub const REFLECTION_PROMPT: &str = "\
Reflect on the tool results above. Inside <reflection>...</reflection> tags:
- Did the action move you toward the goal, or fail / return something unexpected?
- Is the original task complete and verified, or do concrete steps remain?
- If more work is needed, output a new <plan> with updated steps; if the last action failed,
  change approach rather than repeating it.
If the task is fully complete, provide your final answer without any tags.";

/// Whether a model should run the explicit plan/reflect "thinking" scaffolding.
///
/// Reasoning models benefit from the `<plan>`/`<reflection>` protocol. Smaller,
/// non-reasoning models (e.g. Haiku, flash/mini/fast variants) tend to spend the
/// turn *narrating* a plan instead of emitting tool calls when handed this
/// ceremony, so they do better acting directly. Fast/small markers take
/// precedence; unknown models default to direct-acting for tool reliability.
#[must_use]
pub fn model_uses_reasoning(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    const DIRECT: &[&str] = &[
        "mini", "haiku", "flash", "fast", "lite", "-v3", "-v4", "llama", "gpt-3.5", "gpt-4o",
    ];
    if DIRECT.iter().any(|marker| m.contains(marker)) {
        return false;
    }
    const REASONING: &[&str] = &[
        "-r1",
        "reasoning",
        "thinking",
        "o4-",
        "grok-4",
        "opus",
        "sonnet",
        "gpt-5",
        "gemini-3.1-pro",
    ];
    REASONING.iter().any(|marker| m.contains(marker))
}

/// True when the client sends `reasoning_effort` (low/medium/high). Upstream thinking
/// mode (e.g. Alibaba/Qwen) rejects `tool_choice=required`.
#[must_use]
pub fn provider_thinking_mode_active(reasoning_effort: Option<&str>) -> bool {
    let Some(v) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    matches!(
        v.to_ascii_lowercase().as_str(),
        "low" | "medium" | "high"
    )
}

/// Whether the agent may send `tool_choice=required` to nudge tool use.
#[must_use]
pub fn may_force_tool_choice(
    reasoning_effort: Option<&str>,
    enable_thinking: bool,
    model: &str,
) -> bool {
    if provider_thinking_mode_active(reasoning_effort) {
        return false;
    }
    !(enable_thinking && model_uses_reasoning(model))
}

/// Detected structured block from an LLM response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingBlock {
    Plan(String),
    Thinking(String),
    Reflection(String),
    /// Regular text content (not inside any tag).
    Text(String),
}

/// Parse a completed LLM response into structured blocks.
pub fn parse_thinking_blocks(text: &str) -> Vec<ThinkingBlock> {
    let mut blocks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let candidates: Vec<(&str, Option<TagExtraction<'_>>)> = ["plan", "thinking", "reflection"]
            .iter()
            .map(|tag| (*tag, try_extract_tag(remaining, tag)))
            .collect();

        let earliest = candidates
            .into_iter()
            .filter_map(|(tag, ext)| ext.map(|e| (tag, e)))
            .min_by_key(|(_, e)| e.before.len());

        if let Some((tag, result)) = earliest {
            if !result.before.is_empty() {
                blocks.push(ThinkingBlock::Text(result.before.to_string()));
            }
            match tag {
                "plan" => blocks.push(ThinkingBlock::Plan(result.content.to_string())),
                "thinking" => blocks.push(ThinkingBlock::Thinking(result.content.to_string())),
                "reflection" => blocks.push(ThinkingBlock::Reflection(result.content.to_string())),
                _ => {}
            }
            remaining = result.after;
        } else {
            blocks.push(ThinkingBlock::Text(remaining.to_string()));
            break;
        }
    }

    blocks
}

/// Returns true if the reflection contains a new `<plan>` block,
/// indicating the agent wants to re-plan and continue.
pub fn reflection_has_replan(reflection: &str) -> bool {
    reflection.contains("<plan>")
}

struct TagExtraction<'a> {
    before: &'a str,
    content: &'a str,
    after: &'a str,
}

fn try_extract_tag<'a>(text: &'a str, tag: &str) -> Option<TagExtraction<'a>> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let open_pos = text.find(&open)?;
    let content_start = open_pos + open.len();
    let close_pos = text[content_start..].find(&close)?;

    Some(TagExtraction {
        before: &text[..open_pos],
        content: &text[content_start..content_start + close_pos],
        after: &text[content_start + close_pos + close.len()..],
    })
}

/// State machine for tracking which tag we're inside during streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPhase {
    Normal,
    Plan,
    Thinking,
    Reflection,
}

impl StreamPhase {
    /// Given newly accumulated content, detect if we've entered or exited a tag.
    #[must_use]
    pub fn update(self, accumulated: &str) -> Self {
        let in_plan = is_inside_tag(accumulated, "plan");
        let in_thinking = is_inside_tag(accumulated, "thinking");
        let in_reflection = is_inside_tag(accumulated, "reflection");

        if in_plan {
            Self::Plan
        } else if in_thinking {
            Self::Thinking
        } else if in_reflection {
            Self::Reflection
        } else {
            Self::Normal
        }
    }
}

fn is_inside_tag(text: &str, tag: &str) -> bool {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let mut inside = false;
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find(&open) {
        let abs_pos = search_from + pos + open.len();
        inside = true;
        if let Some(close_pos) = text[abs_pos..].find(&close) {
            inside = false;
            search_from = abs_pos + close_pos + close.len();
        } else {
            break;
        }
    }

    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_block() {
        let text = "Some text <plan>Step 1\nStep 2</plan> more text";
        let blocks = parse_thinking_blocks(text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], ThinkingBlock::Text("Some text ".to_string()));
        assert_eq!(
            blocks[1],
            ThinkingBlock::Plan("Step 1\nStep 2".to_string())
        );
        assert_eq!(blocks[2], ThinkingBlock::Text(" more text".to_string()));
    }

    #[test]
    fn parse_multiple_blocks() {
        let text = "<thinking>hmm</thinking><plan>do X</plan>";
        let blocks = parse_thinking_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0],
            ThinkingBlock::Thinking("hmm".to_string())
        );
        assert_eq!(blocks[1], ThinkingBlock::Plan("do X".to_string()));
    }

    #[test]
    fn no_tags() {
        let text = "Just plain text";
        let blocks = parse_thinking_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            ThinkingBlock::Text("Just plain text".to_string())
        );
    }

    #[test]
    fn reflection_with_replan() {
        let r = "The file was not found. <plan>Try a different path</plan>";
        assert!(reflection_has_replan(r));
    }

    #[test]
    fn stream_phase_tracking() {
        assert_eq!(StreamPhase::Normal.update("hello"), StreamPhase::Normal);
        assert_eq!(
            StreamPhase::Normal.update("hello <plan>step"),
            StreamPhase::Plan
        );
        assert_eq!(
            StreamPhase::Plan.update("hello <plan>step</plan> done"),
            StreamPhase::Normal
        );
    }

    #[test]
    fn provider_thinking_blocks_forced_tool_choice() {
        assert!(provider_thinking_mode_active(Some("medium")));
        assert!(!provider_thinking_mode_active(Some("off")));
        assert!(!may_force_tool_choice(Some("high"), false, "qwen-flash"));
    }
}
