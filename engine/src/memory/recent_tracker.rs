use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Delta, DiffOptions, Repository};

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct RecentChange {
    pub file_path: String,
    pub commit_hash: String,
    pub timestamp: i64,
    pub author: String,
    pub message: String,
    pub change_type: ChangeType,
    pub lines_added: usize,
    pub lines_deleted: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone)]
pub struct RecentChangesTracker {
    repo_path: std::path::PathBuf,
    cache: HashMap<String, Vec<RecentChange>>,
    cache_ttl_secs: i64,
    last_cache_time: i64,
}

impl RecentChangesTracker {
    pub fn new(repo_path: &Path) -> Result<Self, AppError> {
        // Verify the path exists and is a git repo
        Repository::open(repo_path).map_err(|e| {
            AppError::InvalidRequest(format!("Failed to open git repository: {}", e))
        })?;

        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            cache: HashMap::new(),
            cache_ttl_secs: 300,
            last_cache_time: 0,
        })
    }

    pub fn get_recent_changes(
        &mut self,
        max_commits: usize,
    ) -> Result<Vec<RecentChange>, AppError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        if now - self.last_cache_time > self.cache_ttl_secs {
            self.refresh_cache(max_commits)?;
            self.last_cache_time = now;
        }

        let mut all_changes: Vec<RecentChange> = self.cache.values().flatten().cloned().collect();
        all_changes.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        Ok(all_changes)
    }

    pub fn get_file_changes(
        &mut self,
        file_path: &str,
        max_commits: usize,
    ) -> Result<Vec<RecentChange>, AppError> {
        if self.cache.is_empty() {
            self.refresh_cache(max_commits)?;
        }

        Ok(self.cache.get(file_path).cloned().unwrap_or_default())
    }

    pub fn get_recently_modified_files(
        &mut self,
        max_age_days: i64,
    ) -> Result<Vec<String>, AppError> {
        let changes = self.get_recent_changes(100)?;
        let cutoff_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - (max_age_days * 24 * 60 * 60);

        let files: HashSet<String> = changes
            .iter()
            .filter(|c| c.timestamp >= cutoff_time && c.change_type != ChangeType::Deleted)
            .map(|c| c.file_path.clone())
            .collect();

        Ok(files.into_iter().collect())
    }

    pub fn get_file_activity_score(&mut self, file_path: &str) -> Result<f32, AppError> {
        let changes = self.get_file_changes(file_path, 50)?;

        if changes.is_empty() {
            return Ok(0.0);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let total_score: f32 = changes
            .iter()
            .map(|c| {
                let age_days = (now - c.timestamp) / (24 * 60 * 60);
                let recency_weight = 1.0 / (1.0 + age_days as f32 * 0.1);
                let size_weight = ((c.lines_added + c.lines_deleted) as f32).ln().max(1.0);
                recency_weight * size_weight
            })
            .sum();

        Ok(total_score)
    }

    fn refresh_cache(&mut self, max_commits: usize) -> Result<(), AppError> {
        self.cache.clear();

        let repo = Repository::open(&self.repo_path)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to open repository: {}", e)))?;

        let mut revwalk = repo
            .revwalk()
            .map_err(|e| AppError::InvalidRequest(format!("Failed to create revwalk: {}", e)))?;

        revwalk
            .push_head()
            .map_err(|e| AppError::InvalidRequest(format!("Failed to push HEAD: {}", e)))?;

        let mut commit_count = 0;
        for oid in revwalk {
            if commit_count >= max_commits {
                break;
            }

            let oid = oid.map_err(|e| {
                AppError::InvalidRequest(format!("Failed to get commit OID: {}", e))
            })?;

            let commit = repo
                .find_commit(oid)
                .map_err(|e| AppError::InvalidRequest(format!("Failed to find commit: {}", e)))?;

            let changes = self.extract_commit_changes(&repo, &commit)?;

            for change in changes {
                self.cache
                    .entry(change.file_path.clone())
                    .or_insert_with(Vec::new)
                    .push(change);
            }

            commit_count += 1;
        }

        Ok(())
    }

    fn extract_commit_changes(
        &self,
        repo: &Repository,
        commit: &git2::Commit,
    ) -> Result<Vec<RecentChange>, AppError> {
        let mut changes = Vec::new();

        let tree = commit
            .tree()
            .map_err(|e| AppError::InvalidRequest(format!("Failed to get commit tree: {}", e)))?;

        let parent_tree = if commit.parent_count() > 0 {
            Some(
                commit
                    .parent(0)
                    .map_err(|e| {
                        AppError::InvalidRequest(format!("Failed to get parent commit: {}", e))
                    })?
                    .tree()
                    .map_err(|e| {
                        AppError::InvalidRequest(format!("Failed to get parent tree: {}", e))
                    })?,
            )
        } else {
            None
        };

        let mut diff_opts = DiffOptions::new();
        let diff = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))
            .map_err(|e| AppError::InvalidRequest(format!("Failed to create diff: {}", e)))?;

        let commit_hash = format!("{}", commit.id());
        let author = commit.author().name().unwrap_or("Unknown").to_string();
        let message = commit.message().unwrap_or("").to_string();
        let timestamp = commit.time().seconds();

        diff.foreach(
            &mut |delta, _progress| {
                if let Some(change) =
                    self.delta_to_change(&delta, &commit_hash, &author, &message, timestamp)
                {
                    changes.push(change);
                }
                true
            },
            None,
            None,
            None,
        )
        .map_err(|e| AppError::InvalidRequest(format!("Failed to process diff: {}", e)))?;

        Ok(changes)
    }

    fn delta_to_change(
        &self,
        delta: &git2::DiffDelta,
        commit_hash: &str,
        author: &str,
        message: &str,
        timestamp: i64,
    ) -> Option<RecentChange> {
        let file_path = delta.new_file().path()?.to_str()?.to_string();

        let change_type = match delta.status() {
            Delta::Added => ChangeType::Added,
            Delta::Modified => ChangeType::Modified,
            Delta::Deleted => ChangeType::Deleted,
            Delta::Renamed => ChangeType::Renamed,
            _ => return None,
        };

        Some(RecentChange {
            file_path,
            commit_hash: commit_hash.to_string(),
            timestamp,
            author: author.to_string(),
            message: message.to_string(),
            change_type,
            lines_added: 0,
            lines_deleted: 0,
        })
    }

    pub fn get_active_branches(&self) -> Result<Vec<String>, AppError> {
        let mut branches = Vec::new();

        let repo = Repository::open(&self.repo_path)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to open repository: {}", e)))?;

        let branch_iter = repo
            .branches(None)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to get branches: {}", e)))?;

        for branch in branch_iter {
            let (branch, _branch_type) = branch
                .map_err(|e| AppError::InvalidRequest(format!("Failed to read branch: {}", e)))?;

            if let Some(name) = branch.name().ok().flatten() {
                branches.push(name.to_string());
            }
        }

        Ok(branches)
    }

    pub fn get_current_branch(&self) -> Result<String, AppError> {
        let repo = Repository::open(&self.repo_path)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to open repository: {}", e)))?;

        let head = repo
            .head()
            .map_err(|e| AppError::InvalidRequest(format!("Failed to get HEAD: {}", e)))?;

        if let Some(name) = head.shorthand() {
            Ok(name.to_string())
        } else {
            Ok("detached HEAD".to_string())
        }
    }
}

pub fn format_recent_changes(changes: &[RecentChange], max_items: usize) -> String {
    let mut output = String::new();

    output.push_str("## Recent Changes\n\n");

    for (idx, change) in changes.iter().take(max_items).enumerate() {
        let change_icon = match change.change_type {
            ChangeType::Added => "➕",
            ChangeType::Modified => "✏️",
            ChangeType::Deleted => "🗑️",
            ChangeType::Renamed => "🔄",
        };

        output.push_str(&format!(
            "{}. {} **{}** by {} ({} ago)\n",
            idx + 1,
            change_icon,
            change.file_path,
            change.author,
            format_relative_time(change.timestamp)
        ));

        if !change.message.is_empty() {
            let first_line = change.message.lines().next().unwrap_or("");
            output.push_str(&format!("   > {}\n", first_line));
        }

        output.push('\n');
    }

    output
}

fn format_relative_time(timestamp: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let diff = now - timestamp;

    if diff < 60 {
        format!("{}s", diff)
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}
