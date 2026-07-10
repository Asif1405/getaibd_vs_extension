use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
use crate::tools::{tmpfile, Tool};

/// Fetch the contents of a URL — GitHub repos/PRs/issues/files/commits (via the
/// authenticated GitHub CLI, so private repos work too) or any general web page.
///
/// This is the agent's canonical way to "open a link". It replaces ad-hoc
/// `curl`/`wget`, which run anonymously and 404 on private GitHub resources.
/// Fetched content is spilled to `.getaibd/tmp/` and returned as a path +
/// preview so it stays out of the model's context.
pub struct WebFetch {
    root: Arc<PathBuf>,
    client: reqwest::Client,
}

impl WebFetch {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self {
            root,
            client: reqwest::Client::builder()
                .user_agent("getaibd-agent")
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch the contents of a URL and return it as text. Use this to read a web page, \
         documentation, or a GitHub repo/PR/issue/file/commit — instead of curl/wget. \
         For github.com links it first uses a LOCAL CLONE of the repo if one exists \
         (project root or a sibling dir), fetching PR/commit diffs over your working \
         git/SSH credentials — so PRIVATE repos work just like they do in your IDE. \
         Otherwise it falls back to the authenticated GitHub CLI (which also honours \
         GH_TOKEN/GITHUB_TOKEN). For any other URL it performs an HTTP GET and returns \
         readable text."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch (web page, or a github.com repo/PR/issue/file/commit URL)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let url = input["url"].as_str().unwrap_or("").trim().to_string();
        if url.is_empty() {
            return Err(AppError::InvalidRequest("url is required".into()));
        }

        let (host, path) = split_host_path(&url);

        let (source, content) = if is_github_host(&host) {
            fetch_github(&self.root, &host, &path, &url).await?
        } else {
            ("web", fetch_web(&self.client, &url).await?)
        };

        // Spill to a temp file so large PRs/diffs/pages stay out of context.
        let ext = if source.ends_with("pr") || source.ends_with("commit") {
            "diff"
        } else {
            "txt"
        };
        let mut payload = tmpfile::stash(&self.root, source, ext, &content).await;
        payload["url"] = json!(url);
        payload["source"] = json!(source);
        Ok(payload)
    }
}

/// Split a URL into `(host, path)` without pulling in a URL-parser dependency.
/// `path` keeps its leading `/` and drops any `?query`/`#fragment`.
fn split_host_path(url: &str) -> (String, String) {
    let no_scheme = url
        .splitn(2, "://")
        .nth(1)
        .unwrap_or(url);
    let (authority, rest) = match no_scheme.find('/') {
        Some(i) => (&no_scheme[..i], &no_scheme[i..]),
        None => (no_scheme, "/"),
    };
    let host = authority.split('@').next_back().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_lowercase();
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string();
    (host, path)
}

fn is_github_host(host: &str) -> bool {
    host == "github.com"
        || host == "www.github.com"
        || host == "raw.githubusercontent.com"
        || host == "api.github.com"
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

/// Resolve the `gh` binary, falling back to common absolute install paths in
/// case the agent's `PATH` doesn't include Homebrew.
fn gh_bin() -> &'static str {
    for p in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
        if std::path::Path::new(p).exists() {
            return p;
        }
    }
    "gh"
}

/// Run `gh` with the given args, returning stdout on success.
async fn gh(args: &[&str]) -> Result<String, AppError> {
    let out = Command::new(gh_bin())
        .args(args)
        .output()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("failed to run gh (is it installed?): {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(AppError::InvalidRequest(format!(
            "gh {} failed: {}",
            args.join(" "),
            err.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Route a github.com URL to a local clone first (using the user's working git/SSH
/// credentials, like Cursor does when the repo is checked out) and fall back to the
/// authenticated `gh` CLI (which also honours `GH_TOKEN`/`GITHUB_TOKEN`).
async fn fetch_github(
    root: &Path,
    host: &str,
    path: &str,
    url: &str,
) -> Result<(&'static str, String), AppError> {
    // raw.githubusercontent.com/<owner>/<repo>/<ref>/<path...>
    if host == "raw.githubusercontent.com" {
        let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
        if seg.len() >= 4 {
            let (owner, repo, gref) = (seg[0], seg[1], seg[2]);
            let file = seg[3..].join("/");
            if let Some(dir) = find_local_clone(root, owner, repo).await {
                if let Ok(out) = git_local(&dir, &["show", &format!("{gref}:{file}")]).await {
                    return Ok(("github-file", out));
                }
            }
            let api = format!("repos/{owner}/{repo}/contents/{file}?ref={gref}");
            let body = gh(&["api", &api, "-H", "Accept: application/vnd.github.raw"]).await?;
            return Ok(("github-file", body));
        }
    }

    // api.github.com/<...> — pass straight through to gh api.
    if host == "api.github.com" {
        let api = path.trim_start_matches('/');
        return Ok(("github-api", gh(&["api", api]).await?));
    }

    // github.com/<owner>/<repo>/<kind>/<n-or-ref>/<rest...>
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    if seg.len() < 2 {
        return Err(AppError::InvalidRequest(
            "not a recognizable github.com URL".into(),
        ));
    }
    let owner = seg[0];
    let repo = seg[1];
    let slug = format!("{owner}/{repo}");
    let local = find_local_clone(root, owner, repo).await;

    match seg.get(2).copied() {
        Some("pull") | Some("pulls") => {
            let n = seg.get(3).copied().unwrap_or("");
            // Prefer the local clone: fetch the PR ref over the user's SSH creds.
            if let Some(dir) = &local {
                if let Ok(report) = pr_diff_local(dir, n).await {
                    return Ok(("github-pr", report));
                }
            }
            let meta = gh(&[
                "pr", "view", n, "--repo", &slug, "--json",
                "title,state,author,body,additions,deletions,changedFiles,headRefName,baseRefName,url",
            ])
            .await
            .map_err(|e| no_access(url, e))?;
            let diff = gh(&["pr", "diff", n, "--repo", &slug])
                .await
                .unwrap_or_else(|e| format!("(diff unavailable: {e})"));
            let report = format!(
                "# Pull Request {slug}#{n}\n\n## Metadata\n{meta}\n\n## Diff\n{diff}"
            );
            Ok(("github-pr", report))
        }
        Some("issues") | Some("issue") => {
            let n = seg.get(3).copied().unwrap_or("");
            let body = gh(&[
                "issue", "view", n, "--repo", &slug, "--json",
                "title,state,author,body,labels,comments,url",
            ])
            .await
            .map_err(|e| no_access(url, e))?;
            Ok(("github-issue", body))
        }
        Some("blob") if seg.len() >= 5 => {
            let gref = seg[3];
            let file = seg[4..].join("/");
            if let Some(dir) = &local {
                if let Ok(out) = git_local(dir, &["show", &format!("{gref}:{file}")]).await {
                    return Ok(("github-file", out));
                }
            }
            let api = format!("repos/{slug}/contents/{file}?ref={gref}");
            let body = gh(&["api", &api, "-H", "Accept: application/vnd.github.raw"]).await?;
            Ok(("github-file", body))
        }
        Some("commit") | Some("commits") if seg.len() >= 4 => {
            let sha = seg[3];
            if let Some(dir) = &local {
                if let Ok(out) = commit_show_local(dir, sha).await {
                    return Ok(("github-commit", out));
                }
            }
            let api = format!("repos/{slug}/commits/{sha}");
            let body = gh(&["api", &api, "-H", "Accept: application/vnd.github.diff"]).await?;
            Ok(("github-commit", body))
        }
        // Bare repo (or tree/branch view): show the repo overview + README.
        _ => {
            let body = gh(&["repo", "view", &slug])
                .await
                .or_else(|_| Ok::<_, AppError>(String::new()))?;
            if body.trim().is_empty() {
                return Err(no_access(url, AppError::InvalidRequest(String::new())));
            }
            Ok(("github-repo", body))
        }
    }
}

/// Friendly error when neither a local clone nor `gh` could read a private resource.
fn no_access(url: &str, _e: AppError) -> AppError {
    AppError::InvalidRequest(format!(
        "could not read {url}. It's likely private and neither a local clone nor your \
         authenticated GitHub account can access it. Fixes: clone the repo locally (its \
         git/SSH credentials will be used), run `gh auth login` with an account that has \
         access, or set GH_TOKEN/GITHUB_TOKEN for an authorized token."
    ))
}

// --- local-clone helpers ---------------------------------------------------

/// Find a local git clone of `owner/repo`: the project root or any sibling dir
/// whose `.git/config` references that slug (matches https and SSH-alias remotes).
async fn find_local_clone(root: &Path, owner: &str, repo: &str) -> Option<PathBuf> {
    let needle = format!("{}/{}", owner.to_lowercase(), repo.to_lowercase());
    if remote_matches(root, &needle).await {
        return Some(root.to_path_buf());
    }
    let parent = root.parent()?;
    let mut rd = tokio::fs::read_dir(parent).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if p.is_dir() && remote_matches(&p, &needle).await {
            return Some(p);
        }
    }
    None
}

/// True if `dir/.git/config` mentions the `owner/repo` slug in a remote URL.
async fn remote_matches(dir: &Path, needle: &str) -> bool {
    match tokio::fs::read_to_string(dir.join(".git/config")).await {
        Ok(cfg) => cfg.to_lowercase().contains(needle),
        Err(_) => false,
    }
}

/// Run `git` inside a local clone, returning stdout on success.
async fn git_local(dir: &Path, args: &[&str]) -> Result<String, AppError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("git failed: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(AppError::InvalidRequest(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Determine the repo's default base ref (origin/HEAD, else main/master/develop).
async fn default_base(dir: &Path) -> String {
    if let Ok(s) =
        git_local(dir, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]).await
    {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    for b in ["origin/main", "origin/master", "origin/develop"] {
        if git_local(dir, &["rev-parse", "--verify", "--quiet", b]).await.is_ok() {
            return b.to_string();
        }
    }
    "origin/HEAD".to_string()
}

/// Fetch PR `n`'s head over the clone's remote (SSH/creds already work) and build
/// a base...head diff, mirroring what `<pr>.diff` would show.
async fn pr_diff_local(dir: &Path, n: &str) -> Result<String, AppError> {
    let head_ref = format!("refs/getaibd/pr-{n}");
    let spec = format!("refs/pull/{n}/head:{head_ref}");
    git_local(dir, &["fetch", "--no-tags", "--force", "origin", &spec]).await?;
    let base = default_base(dir).await;
    let diff = git_local(dir, &["diff", &format!("{base}...{head_ref}")]).await?;
    let log = git_local(dir, &["log", "--oneline", &format!("{base}..{head_ref}")])
        .await
        .unwrap_or_default();
    Ok(format!(
        "# Pull Request #{n} (via local clone {})\n\n## Commits\n{}\n\n## Diff ({base}...PR head)\n{}",
        dir.display(),
        log.trim(),
        diff
    ))
}

/// Show a commit from the local clone, fetching from origin first if it's absent.
async fn commit_show_local(dir: &Path, sha: &str) -> Result<String, AppError> {
    let present = git_local(dir, &["rev-parse", "--verify", "--quiet", &format!("{sha}^{{commit}}")])
        .await
        .is_ok();
    if !present {
        let _ = git_local(dir, &["fetch", "--no-tags", "origin"]).await;
    }
    git_local(dir, &["show", sha]).await
}

// ---------------------------------------------------------------------------
// Generic web
// ---------------------------------------------------------------------------

async fn fetch_web(client: &reqwest::Client, url: &str) -> Result<String, AppError> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("fetch failed: {e}")))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = resp
        .text()
        .await
        .map_err(|e| AppError::InvalidRequest(format!("failed to read body: {e}")))?;
    if !status.is_success() {
        return Err(AppError::InvalidRequest(format!(
            "fetch returned {status} for {url}"
        )));
    }
    if ctype.contains("html") {
        Ok(html_to_text(&text))
    } else {
        Ok(text)
    }
}

/// Very small HTML→text pass: drop script/style blocks and tags, collapse
/// whitespace. Good enough to feed a model without a full HTML parser dep.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    let mut skip_to: Option<&[u8]> = None;
    while i < bytes.len() {
        if let Some(close) = skip_to {
            if bytes[i..].starts_with(close) {
                i += close.len();
                skip_to = None;
            } else {
                i += 1;
            }
            continue;
        }
        let rest = &html[i..];
        if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case("<script") {
            skip_to = Some(b"</script>");
            i += 7;
            continue;
        }
        if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("<style") {
            skip_to = Some(b"</style>");
            i += 6;
            continue;
        }
        let c = bytes[i] as char;
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            out.push(' ');
        } else if !in_tag {
            out.push(c);
        }
        i += 1;
    }
    // Collapse runs of whitespace into single spaces / newlines.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_ws = false;
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !last_ws {
                collapsed.push(if ch == '\n' { '\n' } else { ' ' });
            }
            last_ws = true;
        } else {
            collapsed.push(ch);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
}
