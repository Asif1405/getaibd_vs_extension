//! General agent conduct rules — scope fidelity, corrections, and stop conditions.
//! Injected each run; not split by domain (git/docker/etc.).

/// Core discipline injected at the start of every agent/debug run.
pub const SCOPE_CONDUCT: &str = r#"## Scope and discipline (always apply)

1. **Follow the user's request literally.** Do exactly what they asked — nothing more, nothing less. If they named files, commands, limits ("only", "staged", "don't"), or steps, match those exactly.
2. **No unstated extra steps.** Do not add staging, refactors, tests, cleanup, or exploration the user did not request. If you believe another step is necessary, use ask_question first.
3. **Corrections override.** When the user corrects or clarifies you, their latest message is the source of truth. Abandon the previous approach; do not repeat the same mistake.
4. **Denied actions are final for this run.** If the user denied a tool or command, do not retry it or an equivalent. Ask or explain what blocked you.
5. **STOP when done.** Once the requested work is complete, stop calling tools. Do not keep "helping" with follow-up work they did not ask for.
6. **ask_question stays in scope.** Options you offer must respect the user's stated limits — do not suggest broadening the task unless they asked for alternatives."#;

/// Note injected when the latest user message looks like a correction.
pub fn correction_note(current: &str, prior_task: &str) -> String {
    format!(
        "The user corrected your approach.\n\
         LATEST MESSAGE (authoritative): \"{current}\"\n\
         PRIOR TASK (for context only): \"{prior_task}\"\n\
         Follow the latest message. Do not repeat steps they rejected or actions they denied."
    )
}

/// True when the user is clarifying, correcting, or narrowing scope — not a casual follow-up.
pub fn is_user_correction(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if lower.len() < 4 {
        return false;
    }
    const MARKERS: &[&str] = &[
        "i said",
        "i asked",
        "not what",
        "that's wrong",
        "that is wrong",
        "you added",
        "you tried",
        "don't ",
        "do not ",
        "only ",
        "just ",
        "already told",
        "why did you",
        "why are you",
        "stop ",
        "wrong",
        "misunderstood",
        "no,",
        "no ",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// True when the user chose to halt via ask_question.
pub fn user_chose_to_stop(answer: &str) -> bool {
    let lower = answer.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    const PHRASES: &[&str] = &[
        "do nothing",
        "don't",
        "dont",
        "stop",
        "cancel",
        "abort",
        "never mind",
        "nevermind",
        "no thanks",
        "leave it",
        "not now",
        "skip",
    ];
    PHRASES.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_correction() {
        assert!(is_user_correction("why did you add all? i said staged"));
        assert!(!is_user_correction("ok"));
    }

    #[test]
    fn detects_stop_answer() {
        assert!(user_chose_to_stop("Do nothing"));
        assert!(!user_chose_to_stop("commit and push"));
    }
}
