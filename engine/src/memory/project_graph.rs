use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct ProjectGraph {
    pub nodes: HashMap<String, FileNode>,
    pub edges: Vec<DependencyEdge>,
}

#[derive(Debug, Clone)]
pub struct FileNode {
    pub path: String,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub export_symbols: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Constant,
    Type,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Visibility {
    Public,
    Private,
    Crate,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub items: Vec<String>,
    pub is_external: bool,
}

#[derive(Debug, Clone)]
pub struct DependencyEdge {
    pub from_file: String,
    pub to_file: String,
    pub import_type: ImportType,
}

#[derive(Debug, Clone)]
pub enum ImportType {
    Direct,
    Indirect,
    Trait,
}

pub struct ProjectGraphBuilder {
    project_root: PathBuf,
    graph: ProjectGraph,
}

impl ProjectGraphBuilder {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            graph: ProjectGraph {
                nodes: HashMap::new(),
                edges: Vec::new(),
            },
        }
    }

    pub fn build(mut self) -> Result<ProjectGraph, AppError> {
        self.scan_rust_files()?;
        self.build_dependency_edges()?;
        Ok(self.graph)
    }

    fn scan_rust_files(&mut self) -> Result<(), AppError> {
        let src_dir = self.project_root.join("src");
        if !src_dir.exists() {
            return Ok(());
        }

        self.walk_directory(&src_dir)?;
        Ok(())
    }

    fn walk_directory(&mut self, dir: &Path) -> Result<(), AppError> {
        for entry in fs::read_dir(dir)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to read directory: {}", e)))?
        {
            let entry = entry.map_err(|e| {
                AppError::InvalidRequest(format!("Failed to read dir entry: {}", e))
            })?;

            let path = entry.path();

            if path.is_dir() {
                self.walk_directory(&path)?;
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                self.process_rust_file(&path)?;
            }
        }

        Ok(())
    }

    fn process_rust_file(&mut self, path: &Path) -> Result<(), AppError> {
        let content = fs::read_to_string(path)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to read file: {}", e)))?;

        let relative_path = path
            .strip_prefix(&self.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let node = self.parse_rust_file(&content, &relative_path)?;
        self.graph.nodes.insert(relative_path.clone(), node);

        Ok(())
    }

    fn parse_rust_file(&self, content: &str, file_path: &str) -> Result<FileNode, AppError> {
        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut export_symbols = HashSet::new();

        let lines: Vec<&str> = content.lines().collect();

        for (line_num, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if let Some(import) = self.parse_use_statement(trimmed) {
                imports.push(import);
            }

            if let Some(symbol) = self.parse_symbol(trimmed, line_num + 1) {
                if symbol.visibility == Visibility::Public {
                    export_symbols.insert(symbol.name.clone());
                }
                symbols.push(symbol);
            }
        }

        Ok(FileNode {
            path: file_path.to_string(),
            symbols,
            imports,
            export_symbols,
        })
    }

    fn parse_use_statement(&self, line: &str) -> Option<Import> {
        if !line.starts_with("use ") {
            return None;
        }

        let use_part = line.strip_prefix("use ")?.trim_end_matches(';').trim();

        let (module, items) = if use_part.contains('{') {
            let parts: Vec<&str> = use_part.split('{').collect();
            if parts.len() != 2 {
                return None;
            }

            let module = parts[0].trim().trim_end_matches("::").to_string();
            let items_str = parts[1].trim().trim_end_matches('}');
            let items: Vec<String> = items_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            (module, items)
        } else {
            let parts: Vec<&str> = use_part.split("::").collect();
            if parts.is_empty() {
                return None;
            }

            let module = parts[..parts.len().saturating_sub(1)].join("::");
            let item = parts.last()?.to_string();

            (module, vec![item])
        };

        let is_external = !module.starts_with("crate") && !module.starts_with("super");

        Some(Import {
            module,
            items,
            is_external,
        })
    }

    fn parse_symbol(&self, line: &str, line_num: usize) -> Option<Symbol> {
        let visibility = if line.starts_with("pub ") {
            Visibility::Public
        } else if line.starts_with("pub(crate)") {
            Visibility::Crate
        } else {
            Visibility::Private
        };

        let line = line
            .trim_start_matches("pub ")
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("async ")
            .trim();

        let (kind, name) = if line.starts_with("fn ") {
            let name = line
                .strip_prefix("fn ")?
                .split('(')
                .next()?
                .trim()
                .to_string();
            (SymbolKind::Function, name)
        } else if line.starts_with("struct ") {
            let name = line
                .strip_prefix("struct ")?
                .split_whitespace()
                .next()?
                .to_string();
            (SymbolKind::Struct, name)
        } else if line.starts_with("enum ") {
            let name = line
                .strip_prefix("enum ")?
                .split_whitespace()
                .next()?
                .to_string();
            (SymbolKind::Enum, name)
        } else if line.starts_with("trait ") {
            let name = line
                .strip_prefix("trait ")?
                .split_whitespace()
                .next()?
                .to_string();
            (SymbolKind::Trait, name)
        } else if line.starts_with("impl ") {
            let name = line
                .strip_prefix("impl ")?
                .split_whitespace()
                .next()?
                .to_string();
            (SymbolKind::Impl, name)
        } else if line.starts_with("mod ") {
            let name = line
                .strip_prefix("mod ")?
                .split_whitespace()
                .next()?
                .to_string();
            (SymbolKind::Module, name)
        } else if line.starts_with("const ") {
            let name = line
                .strip_prefix("const ")?
                .split(':')
                .next()?
                .trim()
                .to_string();
            (SymbolKind::Constant, name)
        } else if line.starts_with("type ") {
            let name = line
                .strip_prefix("type ")?
                .split('=')
                .next()?
                .trim()
                .to_string();
            (SymbolKind::Type, name)
        } else {
            return None;
        };

        Some(Symbol {
            name,
            kind,
            start_line: line_num,
            end_line: line_num,
            visibility,
        })
    }

    fn build_dependency_edges(&mut self) -> Result<(), AppError> {
        let node_paths: Vec<String> = self.graph.nodes.keys().cloned().collect();

        for from_path in &node_paths {
            if let Some(from_node) = self.graph.nodes.get(from_path) {
                for import in &from_node.imports {
                    if import.is_external {
                        continue;
                    }

                    if let Some(to_path) = self.resolve_import(&import.module, from_path) {
                        self.graph.edges.push(DependencyEdge {
                            from_file: from_path.clone(),
                            to_file: to_path,
                            import_type: ImportType::Direct,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn resolve_import(&self, module: &str, _from_file: &str) -> Option<String> {
        if module.starts_with("crate::") {
            let rest = module.strip_prefix("crate::")?;
            let parts: Vec<&str> = rest.split("::").collect();

            let mut path = PathBuf::from("src");
            for part in parts {
                path.push(part);
            }

            let possible_paths = vec![
                format!("{}.rs", path.display()),
                format!("{}/mod.rs", path.display()),
            ];

            for candidate in possible_paths {
                if self.graph.nodes.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }

        None
    }
}

impl ProjectGraph {
    pub fn find_related_files(&self, file_path: &str, max_depth: usize) -> Vec<String> {
        let mut related = HashSet::new();
        let mut visited = HashSet::new();
        let mut to_visit = vec![(file_path.to_string(), 0)];

        while let Some((current, depth)) = to_visit.pop() {
            if depth >= max_depth || visited.contains(&current) {
                continue;
            }

            visited.insert(current.clone());
            related.insert(current.clone());

            for edge in &self.edges {
                if edge.from_file == current && !visited.contains(&edge.to_file) {
                    to_visit.push((edge.to_file.clone(), depth + 1));
                }
                if edge.to_file == current && !visited.contains(&edge.from_file) {
                    to_visit.push((edge.from_file.clone(), depth + 1));
                }
            }
        }

        related.remove(file_path);
        related.into_iter().collect()
    }

    pub fn find_symbol(&self, symbol_name: &str) -> Vec<(String, Symbol)> {
        let mut results = Vec::new();

        for (file_path, node) in &self.nodes {
            for symbol in &node.symbols {
                if symbol.name == symbol_name {
                    results.push((file_path.clone(), symbol.clone()));
                }
            }
        }

        results
    }

    pub fn get_file_dependencies(&self, file_path: &str) -> Vec<String> {
        self.edges
            .iter()
            .filter(|e| e.from_file == file_path)
            .map(|e| e.to_file.clone())
            .collect()
    }

    pub fn get_file_dependents(&self, file_path: &str) -> Vec<String> {
        self.edges
            .iter()
            .filter(|e| e.to_file == file_path)
            .map(|e| e.from_file.clone())
            .collect()
    }
}
