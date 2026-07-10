use std::path::{Path, PathBuf};

use crate::error::AppError;

const HEADER: &str =
    "# Project Memory\n\nPersistent facts, preferences, and patterns learned by the agent.\n\n";

pub struct PersistentMemory {
    path: PathBuf,
}

impl PersistentMemory {
    pub fn new(project_root: &Path) -> Self {
        Self {
            path: crate::paths::resolve_memory_md_path(project_root),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    fn ensure_writable(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    pub fn load(&self) -> Result<String, AppError> {
        if self.path.exists() {
            std::fs::read_to_string(&self.path)
                .map_err(|e| AppError::ProviderError(format!("read MEMORY.md: {e}")))
        } else {
            Ok(String::new())
        }
    }

    pub fn load_facts(&self) -> Result<Vec<Fact>, AppError> {
        let content = self.load()?;
        Ok(parse_facts(&content))
    }

    pub fn add_fact(&self, category: &str, fact: &str) -> Result<(), AppError> {
        let mut content = self.load().unwrap_or_default();
        if content.is_empty() {
            content = HEADER.to_string();
        }

        let section_header = format!("## {category}\n");
        if let Some(pos) = content.find(&section_header) {
            let insert_at = pos + section_header.len();
            let entry = format!("- {fact}\n");
            content.insert_str(insert_at, &entry);
        } else {
            use std::fmt::Write;
            let _ = write!(content, "\n## {category}\n\n- {fact}\n");
        }

        self.ensure_writable();
        std::fs::write(&self.path, &content)
            .map_err(|e| AppError::ProviderError(format!("write MEMORY.md: {e}")))
    }

    pub fn remove_fact(&self, category: &str, fact_substr: &str) -> Result<bool, AppError> {
        let content = self.load()?;
        let mut lines: Vec<&str> = content.lines().collect();
        let mut in_section = false;
        let mut removed = false;

        lines.retain(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("## ") {
                in_section = trimmed.strip_prefix("## ").unwrap_or("") == category;
            }
            if in_section && trimmed.starts_with("- ") && trimmed.contains(fact_substr) {
                removed = true;
                return false;
            }
            true
        });

        if removed {
            let new_content = lines.join("\n") + "\n";
            self.ensure_writable();
            std::fs::write(&self.path, new_content)
                .map_err(|e| AppError::ProviderError(format!("write MEMORY.md: {e}")))?;
        }
        Ok(removed)
    }

    pub fn as_context(&self) -> String {
        self.load().unwrap_or_default()
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub category: String,
    pub content: String,
}

fn parse_facts(content: &str) -> Vec<Fact> {
    let mut facts = Vec::new();
    let mut current_category = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(cat) = trimmed.strip_prefix("## ") {
            current_category = cat.to_string();
        } else if let Some(fact) = trimmed.strip_prefix("- ") {
            if !current_category.is_empty() {
                facts.push(Fact {
                    category: current_category.clone(),
                    content: fact.to_string(),
                });
            }
        }
    }

    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        let facts = parse_facts("");
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_with_sections() {
        let md = "# Memory\n\n## Preferences\n\n- Use tabs\n- Dark mode\n\n## Patterns\n\n- Always run tests\n";
        let facts = parse_facts(md);
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[0].category, "Preferences");
        assert_eq!(facts[0].content, "Use tabs");
        assert_eq!(facts[2].category, "Patterns");
    }
}
