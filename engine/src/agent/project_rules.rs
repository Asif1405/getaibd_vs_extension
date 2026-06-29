//! Load project instructions from `.getaibd/` (AGENTS.md, rules, skills, MEMORY).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};

/// Built-in baseline agent guidance, applied to every project automatically.
///
/// A project's own `.getaibd/AGENTS.md` takes precedence: any `##` section it
/// defines overrides the baseline, and only the sections it omits fall back to
/// these defaults. The baseline is intentionally tool-agnostic — concrete
/// build/test commands belong in the project's own file.
const BASELINE_AGENTS: &str = r#"## Golden rule — verify before you commit

Never run `git commit` until checks for the code you touched pass, in this order:
format → lint → types → tests → build → pre-commit hooks. If you broke it, fix it
before moving on. Don't bypass hooks (`--no-verify`) or disable rules to go green.

## Discover, don't assume

Project structure and tooling vary. Before acting, find the project's own commands
and conventions instead of guessing:

- Read the manifest/scripts: `package.json`, `pyproject.toml`/`Makefile`,
  `Cargo.toml`, `go.mod`, `build.gradle`, CI config, and any linter/formatter config.
- Reuse the existing runner (the repo's lint/test/build scripts) — don't invent new
  commands or tools when the project already defines them.
- Match the surrounding code's style, patterns, and libraries.

## Find code by grepping first

- To locate relevant code, run `grep`/`rg` FIRST via run_command, then read only the files
  that matched. Build the pattern as a case-insensitive regex alternation (`rg -ni "a|b|c"`)
  that deliberately BROADENS coverage beyond the literal words in the request:
    * the key terms / identifiers from the issue itself;
    * synonyms and related domain words (e.g. for "auth": `auth|login|signin|session|
      credential|token|permission`);
    * antonyms / opposite-state words — paired behaviour almost always lives in the same
      file, so a bug about one half is found by grepping the other: `disable`→also `enable`,
      `hide`→`show`, `remove|delete`→`add|create|insert`, `expand`→`collapse`, `open`→`close`,
      `start`→`stop`, `lock`→`unlock`, `mute`→`unmute`;
    * naming variants of the same concept: camelCase / snake_case / kebab-case and
      singular/plural (e.g. `userId|user_id|user-id|users`).
  Example: `rg -ni "redirect|forward|handle_no_permission|X-Up-Location"`.
- `grep`/`rg` is the primary way to find where things live. The `search_files` tool is a
  secondary fallback — use it only when a shell grep isn't available or convenient.
- Don't read or list directories exhaustively to "discover" where something lives — narrow
  with a grep, then open the handful of hits. Widen or rephrase the pattern (related names,
  call sites, imports, config keys) before falling back to broad reading.

## Confirm a path exists before you touch it

- Before you `read_file`, `list_directory`, `patch_file`, `move_file`, or `delete_file` a
  path, make sure it actually exists — don't act on a guessed or assumed location. Either
  the path came from a real signal (a grep/`rg` hit, a prior listing, an import you read, or
  a path the user gave you), or you verify it first with a quick `ls`/`stat` or
  `rg --files -g '<name>'`.
- If the check shows the path is missing, don't retry blindly: grep for the real name
  (it may have moved or be spelled differently) or list the parent directory to find it.
  Only `write_file` a brand-new path once you've confirmed the parent directory exists.

## Run commands efficiently

Pick the command that does the job in the fewest, fastest steps — every command is a
round-trip, so favour the ones that return the answer directly.

- Commands run WITHOUT a shell: no pipes (`|`), redirects (`>`/`>>`), chaining (`&&`/`;`),
  or glob expansion. Put the program in `command` and each argument in `args`, one command
  per call. Use a tool's own flags instead of piping — e.g. `rg -l` (names only),
  `rg -m 20` (cap matches), `rg -c` (count) rather than piping to `head`/`wc`.
- Search with `rg`, not `grep -r`, `find … -exec grep`, or reading whole directories: it's
  far faster and skips `.git`, ignored, and binary files by default. Find files by name with
  `rg --files -g '<glob>'` instead of recursive `find` or `ls -R`.
- Read the minimum. Check size first (`wc -l`, or `rg -c <pat>`), then jump to the relevant
  lines (`rg -n -A3 -B3 <pat>`) instead of `cat`-ing a large file end to end. Less output is
  faster and cheaper.
- Scope tests and builds tightly: run the single affected test (e.g.
  `pytest path/to/test.py::test_x`, `cargo test <name>`, `go test ./pkg -run TestX`,
  `npm test -- <file>`) while iterating, and rely on incremental/cached builds. Run the full
  suite once at the end, not after every edit.
- Don't repeat work: never re-run a build, test, or search that already passed — reuse the
  earlier output with `read_terminal`. Prefer the dedicated tools (`read_file`,
  `search_files`, `git_status`/`git_diff`/`git_log`) over shelling out where they're
  equivalent; they're lower-overhead and need no approval.
- Set a tight `timeout_secs` so a quick command that hangs fails fast. Start long-running
  processes (dev servers, watchers, `tail -f`) and move on — they keep running in their own
  terminal; read their output later with `read_terminal` instead of blocking on them.

## Stay in the project's own code

- Don't `grep`, `read_file`, `list_directory`, or `run_command` inside dependency,
  virtualenv, or build dirs: `.venv/`, `venv/`, `env/`, `site-packages/`,
  `node_modules/`, `vendor/`, `target/`, `dist/`, `build/`, `__pycache__/`. Rely on
  your own knowledge of those libraries' public APIs.
- When you DO need current third-party facts — the latest version of a package, how a
  library's API works, a changelog, or what an error message means — use the
  `web_search` tool instead of digging through vendored dependency source. Search the
  web; don't read `.venv`/`node_modules`.
- Only inspect installed third-party source when the user explicitly asks, or a bug
  clearly traces into one specific library file — and then read just that file.

## Environment errors when running commands

- If a command fails with an environment/config error (missing or unset variable,
  "KeyError"/"undefined env", failed DB/service connection, missing API key, wrong
  port/host, "command not found" for a project tool), check the project's env files
  BEFORE guessing or asking: `.env`, `.env.local`, `.env.example`/`.env.sample`,
  `.env.<environment>`, plus env sections in `docker-compose.yml`, `Makefile`, CI
  config, and the runner scripts.
- Compare what the failing command needs against what's defined. If a variable is only
  in `.env.example`, the real `.env` is likely missing it — surface exactly which keys
  are absent so the user can fill them. Prefer running the command through the project's
  own loader (e.g. `dotenv`, `npm run`, `poetry run`, `make`) so the env is applied.
- Read env files to understand required keys, but never print, log, or commit their
  secret values.

## Make changes small and focused

- Smallest change that fully solves the task; avoid drive-by refactors.
- Don't leave dead code, commented-out blocks, or debug prints.
- Comments explain why (intent, trade-offs, constraints), never narrate what.

## Testing

- If the touched code has tests, or a test is reasonable to add, write or update a
  test for the change — new behavior or a bug fix without a test is incomplete.
- Put tests where the project already puts them; follow its naming and helpers.
- Run the relevant tests after each change, not only at the end.
- Cover the edge cases and the failure path, not just the happy path.

## Correctness & robustness

- Handle errors explicitly; don't swallow exceptions or ignore returned errors.
- Validate inputs at boundaries (API, CLI, user data); fail loudly and early.
- Mind concurrency, resource cleanup, and off-by-one/null/empty cases.
- Preserve backward compatibility unless the task says otherwise.

## Security

- Never commit secrets (`.env`, keys, tokens, credentials). Use env/secret stores.
- Never log secrets or PII. Treat all external input as untrusted.
- Don't weaken auth, CORS, or crypto to make something work — ask instead.

## Version control hygiene

- Commit only when asked; keep commits atomic with a clear why in the message.
- Don't commit generated artifacts, build output, or large binaries.
- Don't push, force-push, or change git config unless explicitly told.

## When unsure

- If requirements are ambiguous or a choice is the user's to make (scope, schema,
  destructive actions), ask before proceeding. For reversible, low-stakes choices,
  pick a sensible default and note it."#;

/// One skill discovered under `.getaibd/skills/`.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Body after frontmatter (used for auto-inject).
    pub body: String,
}

/// Parsed glob rule from `.getaibd/rules/*.md`.
#[derive(Debug, Clone)]
struct GlobRule {
    description: String,
    globs: Vec<String>,
    always_apply: bool,
    body: String,
}

/// Everything to inject before RAG / file context.
#[derive(Debug, Clone, Default)]
pub struct ProjectInstructions {
    pub system_messages: Vec<String>,
    pub skills: Vec<SkillEntry>,
    /// Full skill body injected when task matches exactly one skill.
    pub auto_skill: Option<SkillEntry>,
}

fn getaibd_dir(root: &Path) -> PathBuf {
    root.join(".getaibd")
}

/// `.getaibd/.gitignore` contents. Ignores generated agent state while leaving
/// authored config (AGENTS.md, rules/, skills/) tracked so it travels with the repo.
const GETAIBD_GITIGNORE: &str = "\
# Managed by GetAIBD — ignore generated agent state.
# Authored config (AGENTS.md, rules/, skills/) is intentionally kept in git.
MEMORY.md
cache/
index/
*.db
*.sqlite
*.sqlite3
";

/// Ensure `.getaibd/` exists and contains a `.gitignore` for generated state.
///
/// Best-effort and idempotent: creates the directory, and only writes the
/// `.gitignore` when it's missing (never clobbers a user-edited one). Call this
/// right before writing any generated file under `.getaibd/`.
pub fn ensure_getaibd_gitignore(project_root: &Path) {
    let dir = getaibd_dir(project_root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(".gitignore");
    if !path.exists() {
        let _ = std::fs::write(&path, GETAIBD_GITIGNORE);
    }
}

fn agents_path(dir: &Path) -> PathBuf {
    dir.join("AGENTS.md")
}

fn memory_path(root: &Path) -> Option<PathBuf> {
    let nested = getaibd_dir(root).join("MEMORY.md");
    if nested.is_file() {
        return Some(nested);
    }
    let legacy = root.join("MEMORY.md");
    if legacy.is_file() {
        return Some(legacy);
    }
    None
}

/// Load MEMORY.md content (`.getaibd/MEMORY.md` preferred, else root `MEMORY.md`).
pub fn load_memory_content(project_root: &Path) -> Option<String> {
    let path = memory_path(project_root)?;
    std::fs::read_to_string(path).ok().filter(|s| !s.trim().is_empty())
}

/// Resolve a skill by name from disk.
pub fn load_skill_by_name(project_root: &Path, name: &str) -> Option<SkillEntry> {
    let skills = discover_skills(project_root);
    skills.into_iter().find(|s| s.name.eq_ignore_ascii_case(name))
}

/// Build all project instruction system messages for an agent run.
pub fn load_project_instructions(
    project_root: &Path,
    workspace_cwd: Option<&Path>,
    user_rules: Option<&str>,
    task: &str,
    hint_paths: &[String],
) -> ProjectInstructions {
    let mut out = ProjectInstructions::default();

    // If the project opted into `.getaibd/`, make sure generated state can't leak
    // into git. We never create the dir ourselves — only guard an existing one.
    if getaibd_dir(project_root).is_dir() {
        ensure_getaibd_gitignore(project_root);
    }

    if let Some(rules) = user_rules.map(str::trim).filter(|s| !s.is_empty()) {
        out.system_messages
            .push(format!("## User rules\n\n{rules}"));
    }

    for chunk in load_agents_chain(project_root, workspace_cwd) {
        out.system_messages.push(chunk);
    }

    for rule in load_matched_glob_rules(project_root, task, hint_paths) {
        out.system_messages.push(rule);
    }

    if let Some(mem) = load_memory_content(project_root) {
        out.system_messages.push(mem);
    }

    out.skills = discover_skills(project_root);
    if let Some(catalog) = format_skills_catalog(&out.skills) {
        out.system_messages.push(catalog);
    }

    out.auto_skill = pick_auto_skill(&out.skills, task);
    if let Some(skill) = &out.auto_skill {
        out.system_messages.push(format!(
            "## Skill: {} (auto-loaded)\n\n{}",
            skill.name, skill.body
        ));
    }

    out
}

fn load_agents_chain(project_root: &Path, workspace_cwd: Option<&Path>) -> Vec<String> {
    let mut chunks = Vec::new();
    // Always inject the baseline; a project's own AGENTS.md overrides per-section
    // and the baseline fills any aspect it leaves out.
    let root_agents = agents_path(&getaibd_dir(project_root));
    let user_text = std::fs::read_to_string(&root_agents).ok();
    chunks.push(format!(
        "## Project\n\n{}",
        merge_agents_with_baseline(user_text.as_deref())
    ));

    let Some(cwd) = workspace_cwd else {
        return chunks;
    };
    let Ok(rel) = cwd.strip_prefix(project_root) else {
        return chunks;
    };
    if rel.as_os_str().is_empty() {
        return chunks;
    }

    let mut dir = project_root.to_path_buf();
    for component in rel.components() {
        dir.push(component);
        let nested = agents_path(&dir.join(".getaibd"));
        if nested.is_file() {
            if let Ok(text) = std::fs::read_to_string(&nested) {
                let t = text.trim();
                if !t.is_empty() {
                    let rel_dir = dir
                        .strip_prefix(project_root)
                        .unwrap_or(&dir)
                        .display();
                    chunks.push(format!("## Scope: {rel_dir}\n\n{t}"));
                }
            }
        }
    }
    chunks
}

/// Merge a project's own AGENTS.md over the built-in baseline.
///
/// The user's file wins: any `##` section it defines is kept verbatim and its
/// matching baseline section is dropped. Baseline sections the user didn't cover
/// are appended so every aspect is always present.
fn merge_agents_with_baseline(user: Option<&str>) -> String {
    let user = user.map(str::trim).filter(|s| !s.is_empty());
    let Some(user) = user else {
        return BASELINE_AGENTS.to_string();
    };

    let covered: HashSet<String> = section_headings(user);
    let missing: Vec<String> = split_sections(BASELINE_AGENTS)
        .into_iter()
        .filter(|(heading, _)| !covered.contains(&normalize_heading(heading)))
        .map(|(_, body)| body)
        .collect();

    if missing.is_empty() {
        return user.to_string();
    }
    format!(
        "{user}\n\n<!-- GetAIBD baseline defaults for aspects not covered above -->\n\n{}",
        missing.join("\n\n")
    )
}

fn normalize_heading(h: &str) -> String {
    // Compare on the heading word(s) before any em-dash qualifier, lowercased.
    let h = h.split(['—', '-']).next().unwrap_or(h);
    h.trim().to_lowercase()
}

fn section_headings(md: &str) -> HashSet<String> {
    split_sections(md)
        .into_iter()
        .map(|(heading, _)| normalize_heading(&heading))
        .collect()
}

/// Split markdown into `(heading_text, full_section_including_heading)` for each
/// top-level `## ` heading. Content before the first heading is ignored.
fn split_sections(md: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in md.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            sections.push((title.trim().to_string(), line.to_string()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push('\n');
            body.push_str(line);
        }
    }
    for (_, body) in sections.iter_mut() {
        *body = body.trim_end().to_string();
    }
    sections
}

fn load_matched_glob_rules(
    project_root: &Path,
    task: &str,
    hint_paths: &[String],
) -> Vec<String> {
    let rules_dir = getaibd_dir(project_root).join("rules");
    if !rules_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&rules_dir) else {
        return Vec::new();
    };

    let mut rules = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if ext != "md" {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(parsed) = parse_rule_file(&raw) else {
            continue;
        };
        rules.push(parsed);
    }

    let mut matched = Vec::new();
    for rule in rules {
        if rule.always_apply || rule_matches(&rule, project_root, task, hint_paths) {
            let title = if rule.description.is_empty() {
                "Project rule".to_string()
            } else {
                rule.description.clone()
            };
            matched.push(format!("## Rule: {title}\n\n{}", rule.body.trim()));
        }
    }
    matched
}

fn parse_rule_file(raw: &str) -> Option<GlobRule> {
    let (front, body) = split_frontmatter(raw)?;
    let mut description = String::new();
    let mut globs = Vec::new();
    let mut always_apply = false;
    for line in front.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("globs:") {
            let g = v.trim().trim_matches('"').to_string();
            if !g.is_empty() {
                globs.push(g);
            }
        } else if let Some(v) = line.strip_prefix("alwaysApply:") {
            always_apply = matches!(v.trim().to_lowercase().as_str(), "true" | "yes" | "1");
        }
    }
    Some(GlobRule {
        description,
        globs,
        always_apply,
        body: body.to_string(),
    })
}

fn rule_matches(rule: &GlobRule, project_root: &Path, task: &str, hint_paths: &[String]) -> bool {
    if rule.globs.is_empty() {
        return false;
    }
    let mut builder = GlobSetBuilder::new();
    for g in &rule.globs {
        if let Ok(glob) = Glob::new(g) {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return false;
    };

    for p in hint_paths {
        let path = Path::new(p);
        let rel = if path.is_absolute() {
            path.strip_prefix(project_root)
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.clone())
        } else {
            p.clone()
        };
        if set.is_match(&rel) {
            return true;
        }
    }

    task.split_whitespace().any(|word| {
        word.len() > 3 && rule.globs.iter().any(|g| g.contains(word))
    }) || set.is_match(task)
}

fn discover_skills(project_root: &Path) -> Vec<SkillEntry> {
    let skills_dir = getaibd_dir(project_root).join("skills");
    if !skills_dir.is_dir() {
        return Vec::new();
    }
    let mut by_name: HashMap<String, SkillEntry> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.is_file() {
                    if let Some(entry) = parse_skill_file(&skill_file) {
                        by_name.insert(entry.name.clone(), entry);
                    }
                }
            } else if path.is_file() {
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Some(entry) = parse_skill_file(&path) {
                        by_name.insert(entry.name.clone(), entry);
                    }
                }
            }
        }
    }

    let mut skills: Vec<SkillEntry> = by_name.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn parse_skill_file(path: &Path) -> Option<SkillEntry> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (front, body) = split_frontmatter(&raw)?;
    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    let mut description = String::new();
    for line in front.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        }
    }
    if description.is_empty() {
        description = name.clone();
    }
    Some(SkillEntry {
        name,
        description,
        path: path.to_path_buf(),
        body: body.trim().to_string(),
    })
}

fn split_frontmatter(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return Some((String::new(), raw.to_string()));
    }
    let rest = trimmed.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');
    Some((front.to_string(), body.to_string()))
}

fn format_skills_catalog(skills: &[SkillEntry]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## Available skills".to_string(),
        String::new(),
        "Call `fetch_skill` with the skill name to load full instructions before acting on a matching task."
            .to_string(),
        String::new(),
    ];
    for s in skills {
        lines.push(format!("- **{}**: {}", s.name, s.description));
    }
    Some(lines.join("\n"))
}

fn pick_auto_skill(skills: &[SkillEntry], task: &str) -> Option<SkillEntry> {
    if skills.is_empty() {
        return None;
    }
    let task_lower = task.to_lowercase();
    let task_tokens: Vec<&str> = task_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .collect();

    let mut scores: Vec<(usize, &SkillEntry)> = skills
        .iter()
        .map(|s| (score_skill_match(s, &task_lower, &task_tokens), s))
        .filter(|(score, _)| *score >= 2)
        .collect();

    scores.sort_by(|a, b| b.0.cmp(&a.0));
    if scores.len() == 1 {
        return Some(scores[0].1.clone());
    }
    if scores.len() >= 2 && scores[0].0 > scores[1].0 {
        return Some(scores[0].1.clone());
    }
    None
}

fn score_skill_match(skill: &SkillEntry, task_lower: &str, task_tokens: &[&str]) -> usize {
    let desc_lower = skill.description.to_lowercase();
    let name_lower = skill.name.to_lowercase();
    let mut score = 0usize;
    if task_lower.contains(&name_lower) {
        score += 3;
    }
    for token in task_tokens {
        if desc_lower.contains(token) {
            score += 1;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn loads_root_agents_and_memory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".getaibd")).unwrap();
        fs::write(root.join(".getaibd/AGENTS.md"), "# Agent rules\nUse pytest.").unwrap();
        fs::write(root.join(".getaibd/MEMORY.md"), "# Memory\n- tabs").unwrap();

        let instr = load_project_instructions(root, None, None, "run tests", &[]);
        assert!(instr
            .system_messages
            .iter()
            .any(|m| m.contains("Use pytest")));
        assert!(instr.system_messages.iter().any(|m| m.contains("tabs")));
    }

    #[test]
    fn baseline_applied_when_no_user_agents() {
        let dir = TempDir::new().unwrap();
        let instr = load_project_instructions(dir.path(), None, None, "do work", &[]);
        let project = instr
            .system_messages
            .iter()
            .find(|m| m.starts_with("## Project"))
            .expect("project chunk present");
        assert!(project.contains("verify before you commit"));
        assert!(project.contains("## Security"));
    }

    #[test]
    fn user_section_overrides_baseline_and_gaps_filled() {
        let merged = merge_agents_with_baseline(Some(
            "## Security\n\nUse our vault only. No exceptions.",
        ));
        // User's Security wins (baseline Security text is not duplicated).
        assert!(merged.contains("Use our vault only"));
        assert!(!merged.contains("Use env/secret stores"));
        // A baseline aspect the user omitted is still present.
        assert!(merged.contains("## Testing"));
    }

    #[test]
    fn no_baseline_block_when_user_covers_everything() {
        let merged = merge_agents_with_baseline(Some(BASELINE_AGENTS));
        assert!(!merged.contains("baseline defaults for aspects not covered"));
    }

    #[test]
    fn ensure_gitignore_writes_once_and_keeps_user_edits() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        ensure_getaibd_gitignore(root);
        let gi = root.join(".getaibd/.gitignore");
        assert!(gi.is_file());
        let body = fs::read_to_string(&gi).unwrap();
        // Active (non-comment) ignore rules.
        let rules: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(rules.contains(&"MEMORY.md"));
        // Authored config must NOT be an ignore rule.
        assert!(!rules.iter().any(|r| r.contains("AGENTS.md") || *r == "rules/" || *r == "skills/"));

        // A user-edited .gitignore is preserved (idempotent, no clobber).
        fs::write(&gi, "custom\n").unwrap();
        ensure_getaibd_gitignore(root);
        assert_eq!(fs::read_to_string(&gi).unwrap(), "custom\n");
    }

    #[test]
    fn memory_fallback_to_root() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("MEMORY.md"), "legacy fact").unwrap();
        assert_eq!(
            load_memory_content(dir.path()).as_deref(),
            Some("legacy fact")
        );
    }

    #[test]
    fn auto_skill_single_match() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".getaibd/skills/release")).unwrap();
        fs::write(
            root.join(".getaibd/skills/release/SKILL.md"),
            "---\nname: release\ndescription: Cut a release and ship VSIX\n---\n# Steps\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".getaibd/skills/other")).unwrap();
        fs::write(
            root.join(".getaibd/skills/other/SKILL.md"),
            "---\nname: other\ndescription: unrelated workflow\n---\n",
        )
        .unwrap();

        let instr = load_project_instructions(root, None, None, "cut a release and ship", &[]);
        assert!(instr.auto_skill.is_some());
        assert_eq!(instr.auto_skill.unwrap().name, "release");
    }

    #[test]
    fn fetch_skill_by_name() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".getaibd/skills/foo")).unwrap();
        fs::write(
            root.join(".getaibd/skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: Foo skill\n---\nBody here",
        )
        .unwrap();
        let skill = load_skill_by_name(root, "foo").unwrap();
        assert_eq!(skill.body, "Body here");
    }

    #[test]
    fn glob_rule_matches_open_path() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".getaibd/rules")).unwrap();
        fs::create_dir_all(root.join("packages/api/src")).unwrap();
        fs::write(
            root.join(".getaibd/rules/api.md"),
            "---\ndescription: API rules\nglobs: packages/api/**\nalwaysApply: false\n---\nUse FastAPI DI.",
        )
        .unwrap();

        let instr = load_project_instructions(
            root,
            Some(&root.join("packages/api")),
            None,
            "fix endpoint",
            &["packages/api/src/main.py".to_string()],
        );
        assert!(instr
            .system_messages
            .iter()
            .any(|m| m.contains("Use FastAPI DI")));
    }

    #[test]
    fn nested_agents_loaded_for_cwd() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".getaibd")).unwrap();
        fs::write(root.join(".getaibd/AGENTS.md"), "root rules").unwrap();
        fs::create_dir_all(root.join("packages/api/.getaibd")).unwrap();
        fs::write(root.join("packages/api/.getaibd/AGENTS.md"), "api rules").unwrap();

        let instr = load_project_instructions(
            root,
            Some(&root.join("packages/api/src")),
            None,
            "task",
            &[],
        );
        assert!(instr.system_messages.iter().any(|m| m.contains("root rules")));
        assert!(instr.system_messages.iter().any(|m| m.contains("api rules")));
    }
}
