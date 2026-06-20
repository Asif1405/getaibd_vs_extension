/// System prompts and response parsing for plan/think/reflect agent loop.
pub const PLANNING_PROMPT: &str = "\
You are an advanced agent that plans before acting. Follow this protocol:

1. PLAN: Before taking any action, output your plan inside <plan>...</plan> tags.
   Break down the task into clear steps.
2. THINK: When reasoning about a problem, wrap your thoughts in <thinking>...</thinking> tags.
3. ACT: Execute your plan using the available tools.
4. REFLECT: After tool results come back, reflect on the outcome inside <reflection>...</reflection> tags.
   - If the plan succeeded, state what was accomplished.
   - If something failed or needs adjustment, output a new <plan> to re-approach.

Always plan first, then act, then reflect. Adjust your approach based on results.";

pub const REFLECTION_PROMPT: &str = "\
Reflect on the tool results above. Inside <reflection>...</reflection> tags:
- Did the action succeed or fail?
- Is the original task complete, or do more steps remain?
- If more work is needed, output a new <plan> with updated steps.
If the task is fully complete, provide your final answer without any tags.";

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
}
