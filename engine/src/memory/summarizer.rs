use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::error::AppError;

use super::embeddings::EmbeddingProvider;
use super::indexer::MemoryIndexer;
use super::store::MemoryStore;

const IGNORE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "build",
    "__pycache__",
    ".next",
    ".cache",
    "vendor",
    "bin",
    "obj",
    ".idea",
    ".vscode",
    "coverage",
    ".cargo",
];

const KEY_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "Makefile",
    "Dockerfile",
    "docker-compose.yml",
    "README.md",
    "config.toml",
    "config.yaml",
    "config.json",
    ".env.example",
    "tsconfig.json",
    "build.gradle",
    "pom.xml",
    "CMakeLists.txt",
];

const MAX_AUTO_INDEX_FILES: usize = 50;
const MAX_FILE_SIZE_BYTES: u64 = 100_000;

pub struct ProjectSummary {
    pub root: PathBuf,
    pub languages: HashMap<String, LanguageStats>,
    pub total_files: usize,
    pub total_lines: usize,
    pub key_files: Vec<PathBuf>,
    pub entry_points: Vec<PathBuf>,
    pub directory_tree: String,
}

pub struct LanguageStats {
    pub files: usize,
    pub lines: usize,
}

pub fn scan_project(root: &Path) -> Result<ProjectSummary, AppError> {
    let mut languages: HashMap<String, LanguageStats> = HashMap::new();
    let mut total_files = 0;
    let mut total_lines = 0;
    let mut key_files = Vec::new();
    let mut entry_points = Vec::new();
    let mut tree = String::new();

    scan_dir(
        root,
        0,
        &mut languages,
        &mut total_files,
        &mut total_lines,
        &mut key_files,
        &mut entry_points,
        &mut tree,
    )?;

    Ok(ProjectSummary {
        root: root.to_path_buf(),
        languages,
        total_files,
        total_lines,
        key_files,
        entry_points,
        directory_tree: tree,
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_dir(
    path: &Path,
    depth: usize,
    languages: &mut HashMap<String, LanguageStats>,
    total_files: &mut usize,
    total_lines: &mut usize,
    key_files: &mut Vec<PathBuf>,
    entry_points: &mut Vec<PathBuf>,
    tree: &mut String,
) -> Result<(), AppError> {
    if depth > 6 {
        return Ok(());
    }

    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };

    let mut items: Vec<_> = entries.filter_map(Result::ok).collect();
    items.sort_by_key(std::fs::DirEntry::file_name);

    for entry in items {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') && depth == 0 && name != ".env.example" {
            continue;
        }

        if entry_path.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
            let indent = "  ".repeat(depth);
            let _ = writeln!(tree, "{indent}{name}/");
            scan_dir(
                &entry_path,
                depth + 1,
                languages,
                total_files,
                total_lines,
                key_files,
                entry_points,
                tree,
            )?;
        } else if entry_path.is_file() {
            *total_files += 1;

            if depth <= 2 {
                let indent = "  ".repeat(depth);
                let _ = writeln!(tree, "{indent}{name}");
            }

            if KEY_FILES.contains(&name.as_str()) {
                key_files.push(entry_path.clone());
            }

            if is_entry_point(&name) {
                entry_points.push(entry_path.clone());
            }

            if let Some(lang) = detect_lang(&name) {
                let lines = count_lines(&entry_path);
                *total_lines += lines;
                let stats = languages
                    .entry(lang)
                    .or_insert(LanguageStats { files: 0, lines: 0 });
                stats.files += 1;
                stats.lines += lines;
            }
        }
    }

    Ok(())
}

fn is_entry_point(name: &str) -> bool {
    matches!(
        name,
        "main.rs"
            | "lib.rs"
            | "mod.rs"
            | "main.ts"
            | "index.ts"
            | "app.ts"
            | "main.py"
            | "app.py"
            | "__init__.py"
            | "main.go"
            | "Main.java"
            | "main.c"
            | "main.cpp"
    )
}

fn detect_lang(name: &str) -> Option<String> {
    let ext = name.rsplit('.').next()?;
    match ext {
        "rs" => Some("Rust".into()),
        "ts" | "tsx" => Some("TypeScript".into()),
        "js" | "jsx" | "mjs" => Some("JavaScript".into()),
        "py" | "pyi" => Some("Python".into()),
        "go" => Some("Go".into()),
        "java" | "kt" | "scala" => Some("Java/JVM".into()),
        "c" | "h" => Some("C".into()),
        "cpp" | "cxx" | "hpp" => Some("C++".into()),
        "rb" => Some("Ruby".into()),
        "swift" => Some("Swift".into()),
        "zig" => Some("Zig".into()),
        "lua" => Some("Lua".into()),
        "sh" | "bash" | "zsh" => Some("Shell".into()),
        "toml" | "yaml" | "yml" | "json" => Some("Config".into()),
        "md" | "txt" | "rst" => Some("Docs".into()),
        "html" | "css" | "scss" => Some("Web".into()),
        "sql" => Some("SQL".into()),
        _ => None,
    }
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.lines().count())
        .unwrap_or(0)
}

impl ProjectSummary {
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Project Summary\n");
        let _ = writeln!(out, "**Root**: `{}`", self.root.display());
        let _ = writeln!(
            out,
            "**Files**: {} | **Lines**: {}\n",
            self.total_files, self.total_lines
        );

        if !self.languages.is_empty() {
            let _ = writeln!(out, "## Languages\n");
            let mut langs: Vec<_> = self.languages.iter().collect();
            langs.sort_by(|a, b| b.1.lines.cmp(&a.1.lines));
            for (lang, stats) in &langs {
                let _ = writeln!(
                    out,
                    "- **{lang}**: {} files, {} lines",
                    stats.files, stats.lines
                );
            }
            let _ = writeln!(out);
        }

        if !self.key_files.is_empty() {
            let _ = writeln!(out, "## Key Files\n");
            for f in &self.key_files {
                let rel = f.strip_prefix(&self.root).unwrap_or(f);
                let _ = writeln!(out, "- `{}`", rel.display());
            }
            let _ = writeln!(out);
        }

        if !self.entry_points.is_empty() {
            let _ = writeln!(out, "## Entry Points\n");
            for f in &self.entry_points {
                let rel = f.strip_prefix(&self.root).unwrap_or(f);
                let _ = writeln!(out, "- `{}`", rel.display());
            }
            let _ = writeln!(out);
        }

        if !self.directory_tree.is_empty() {
            let _ = writeln!(out, "## Directory Structure\n");
            let _ = writeln!(out, "```");
            out.push_str(&self.directory_tree);
            let _ = writeln!(out, "```");
        }

        out
    }

    pub fn important_files(&self) -> Vec<&Path> {
        let mut files: Vec<&Path> = Vec::new();
        for f in &self.entry_points {
            files.push(f);
        }
        for f in &self.key_files {
            if !files.contains(&f.as_path()) {
                files.push(f);
            }
        }
        files.truncate(MAX_AUTO_INDEX_FILES);
        files
    }
}

pub async fn summarize_and_index(
    root: &Path,
    store: &MemoryStore,
    embedder: &dyn EmbeddingProvider,
) -> Result<ProjectSummary, AppError> {
    let summary = scan_project(root)?;
    let markdown = summary.to_markdown();

    let indexer = MemoryIndexer::new(store, embedder);

    indexer
        .index_persistent_fact("project_summary", &markdown)
        .await?;

    let mut auto_count = 0;
    for file_path in summary.important_files() {
        if auto_count >= MAX_AUTO_INDEX_FILES {
            break;
        }

        let Ok(meta) = std::fs::metadata(file_path) else {
            continue;
        };
        if meta.len() > MAX_FILE_SIZE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file_path) else {
            continue;
        };

        let rel = file_path.strip_prefix(root).unwrap_or(file_path);
        if indexer.index_file(rel, &content).await.is_ok() {
            auto_count += 1;
        }
    }

    tracing::info!(
        "Project summarized: {} files, {} lines, {} languages, {} files auto-indexed",
        summary.total_files,
        summary.total_lines,
        summary.languages.len(),
        auto_count
    );

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_temp_project() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(
            src.join("main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\n",
        )
        .unwrap();

        let summary = scan_project(dir.path()).unwrap();
        assert_eq!(summary.total_files, 3);
        assert!(summary.total_lines >= 4);
        assert!(summary.languages.contains_key("Rust"));
        assert!(!summary.key_files.is_empty());
        assert!(!summary.entry_points.is_empty());
    }

    #[test]
    fn ignores_target_and_node_modules() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target").join("junk.rs"), "junk").unwrap();
        fs::create_dir(dir.path().join("node_modules")).unwrap();
        fs::write(dir.path().join("node_modules").join("pkg.js"), "pkg").unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let summary = scan_project(dir.path()).unwrap();
        assert_eq!(summary.total_files, 1);
    }

    #[test]
    fn summary_to_markdown_contains_sections() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();

        let summary = scan_project(dir.path()).unwrap();
        let md = summary.to_markdown();
        assert!(md.contains("# Project Summary"));
        assert!(md.contains("Rust"));
        assert!(md.contains("Cargo.toml"));
    }

    #[test]
    fn important_files_includes_entry_points_and_key_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.path().join("README.md"), "# Test\n").unwrap();

        let summary = scan_project(dir.path()).unwrap();
        let important = summary.important_files();
        assert!(important.len() >= 2);
    }
}
