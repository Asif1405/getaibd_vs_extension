/// Returns the system prompt that forces the LLM to return structured patch JSON.
pub fn patch_system_prompt() -> &'static str {
    r#"You are a precise code editing assistant. When asked to make changes, you MUST respond with a JSON object in this exact format:

```json
{
  "plan": "Brief explanation of what you're changing and why",
  "edits": [
    {
      "file": "relative/path/to/file.rs",
      "operation": "modify",
      "diff": "--- a/relative/path/to/file.rs\n+++ b/relative/path/to/file.rs\n@@ -10,7 +10,7 @@\n context line\n-old line\n+new line\n context line"
    },
    {
      "file": "new/file.rs",
      "operation": "create",
      "content": "full file content here"
    }
  ],
  "verify_commands": ["cargo check", "cargo test"]
}
```

Rules:
1. ALWAYS return valid JSON wrapped in ```json code blocks
2. Use unified diff format for "modify" operations (context lines are required)
3. For new files, use "create" with full "content"
4. For deletions, use "delete" with no diff or content
5. File paths MUST be relative to the project root
6. Include 3 lines of context in diffs to ensure correct application
7. Make minimal changes — only modify what is necessary
8. Order edits by dependency (create dependencies before files that use them)

NEVER return raw code outside of this JSON structure."#
}

/// Wraps user prompt for patch mode
pub fn patch_user_prompt(task: &str, context: &str) -> String {
    format!(
        "Project context:\n{}\n\nTask: {}\n\nRespond with the structured JSON patch.",
        context, task
    )
}

/// Extracts JSON from LLM response (handles ```json ... ``` wrapping)
pub fn extract_patch_json(response: &str) -> Option<&str> {
    // Try ```json ... ``` block first
    if let Some(start) = response.find("```json") {
        let after = &response[start + 7..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    // Try ``` ... ``` block
    if let Some(start) = response.find("```") {
        let after = &response[start + 3..];
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('{') {
                return Some(inner);
            }
        }
    }
    // Try raw JSON
    let trimmed = response.trim();
    if trimmed.starts_with('{') {
        return Some(trimmed);
    }
    None
}
