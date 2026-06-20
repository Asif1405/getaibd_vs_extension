use std::path::Path;

#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub content: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: ChunkKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkKind {
    Function,
    Class,
    Impl,
    Module,
    Other,
}

pub fn chunk_code(source: &str, file_path: &Path, max_lines: usize) -> Vec<CodeChunk> {
    let lang = detect_language(file_path);
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let boundaries = find_boundaries(&lines, lang);
    if boundaries.is_empty() {
        return fallback_chunk(&lines, max_lines);
    }

    merge_chunks(&lines, &boundaries, max_lines)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    Other,
}

fn detect_language(path: &Path) -> Language {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Language::Rust,
        Some("ts" | "tsx" | "js" | "jsx" | "mjs") => Language::TypeScript,
        Some("py" | "pyi") => Language::Python,
        Some("go") => Language::Go,
        Some("java" | "kt" | "scala") => Language::Java,
        _ => Language::Other,
    }
}

struct Boundary {
    line: usize,
    kind: ChunkKind,
}

fn find_boundaries(lines: &[&str], lang: Language) -> Vec<Boundary> {
    let mut boundaries = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let kind = match lang {
            Language::Rust => detect_rust_boundary(trimmed),
            Language::TypeScript => detect_ts_boundary(trimmed),
            Language::Python => detect_python_boundary(trimmed),
            Language::Go => detect_go_boundary(trimmed),
            Language::Java => detect_java_boundary(trimmed),
            Language::Other => detect_generic_boundary(trimmed),
        };

        if let Some(k) = kind {
            boundaries.push(Boundary { line: i, kind: k });
        }
    }

    boundaries
}

fn detect_rust_boundary(trimmed: &str) -> Option<ChunkKind> {
    if starts_with_any(
        trimmed,
        &[
            "pub fn ",
            "fn ",
            "pub async fn ",
            "async fn ",
            "pub(crate) fn ",
        ],
    ) {
        return Some(ChunkKind::Function);
    }
    if starts_with_any(
        trimmed,
        &["impl ", "pub struct ", "struct ", "pub enum ", "enum "],
    ) {
        return Some(ChunkKind::Impl);
    }
    if starts_with_any(trimmed, &["pub mod ", "mod "]) {
        return Some(ChunkKind::Module);
    }
    if starts_with_any(trimmed, &["pub trait ", "trait "]) {
        return Some(ChunkKind::Class);
    }
    None
}

fn detect_ts_boundary(trimmed: &str) -> Option<ChunkKind> {
    if starts_with_any(
        trimmed,
        &[
            "function ",
            "export function ",
            "async function ",
            "export async function ",
            "const ",
            "export const ",
            "let ",
            "export let ",
        ],
    ) && (trimmed.contains("=>") || trimmed.contains('('))
    {
        return Some(ChunkKind::Function);
    }
    if starts_with_any(
        trimmed,
        &["class ", "export class ", "export default class "],
    ) {
        return Some(ChunkKind::Class);
    }
    if starts_with_any(
        trimmed,
        &["interface ", "export interface ", "type ", "export type "],
    ) {
        return Some(ChunkKind::Class);
    }
    None
}

fn detect_python_boundary(trimmed: &str) -> Option<ChunkKind> {
    if starts_with_any(trimmed, &["def ", "async def "]) {
        return Some(ChunkKind::Function);
    }
    if trimmed.starts_with("class ") {
        return Some(ChunkKind::Class);
    }
    None
}

fn detect_go_boundary(trimmed: &str) -> Option<ChunkKind> {
    if trimmed.starts_with("func ") {
        return Some(ChunkKind::Function);
    }
    if starts_with_any(trimmed, &["type ", "struct "]) {
        return Some(ChunkKind::Class);
    }
    None
}

fn detect_java_boundary(trimmed: &str) -> Option<ChunkKind> {
    let method_keywords = ["public ", "private ", "protected ", "static ", "void "];
    if starts_with_any(trimmed, &method_keywords)
        && trimmed.contains('(')
        && !trimmed.contains("class ")
    {
        return Some(ChunkKind::Function);
    }
    if trimmed.contains("class ") || trimmed.contains("interface ") || trimmed.contains("enum ") {
        return Some(ChunkKind::Class);
    }
    None
}

fn detect_generic_boundary(trimmed: &str) -> Option<ChunkKind> {
    if starts_with_any(trimmed, &["fn ", "def ", "func ", "function ", "sub "]) {
        return Some(ChunkKind::Function);
    }
    if starts_with_any(trimmed, &["class ", "struct ", "interface ", "module "]) {
        return Some(ChunkKind::Class);
    }
    None
}

fn starts_with_any(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| s.starts_with(p))
}

fn merge_chunks(lines: &[&str], boundaries: &[Boundary], max_lines: usize) -> Vec<CodeChunk> {
    let mut chunks = Vec::new();

    for (idx, boundary) in boundaries.iter().enumerate() {
        let start = boundary.line;
        let next_start = boundaries.get(idx + 1).map_or(lines.len(), |b| b.line);

        let raw_end = find_block_end(lines, start, next_start);
        let end = raw_end.min(start + max_lines);

        if start < end {
            let content = lines[start..end].join("\n");
            chunks.push(CodeChunk {
                content,
                start_line: start + 1,
                end_line: end,
                kind: boundary.kind.clone(),
            });
        }
    }

    if chunks.is_empty() {
        return fallback_chunk(lines, max_lines);
    }

    let first_boundary = boundaries.first().map_or(0, |b| b.line);
    if first_boundary > 2 {
        let preamble = lines[..first_boundary].join("\n");
        if !preamble.trim().is_empty() {
            chunks.insert(
                0,
                CodeChunk {
                    content: preamble,
                    start_line: 1,
                    end_line: first_boundary,
                    kind: ChunkKind::Other,
                },
            );
        }
    }

    chunks
}

fn find_block_end(lines: &[&str], start: usize, next_boundary: usize) -> usize {
    let start_indent = lines[start].len() - lines[start].trim_start().len();
    let mut brace_depth: i32 = 0;
    let mut found_open = false;
    let end = next_boundary.min(lines.len());

    for (idx, line) in lines[start..end].iter().enumerate() {
        let i = start + idx;
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
                found_open = true;
            } else if ch == '}' {
                brace_depth -= 1;
            }
        }

        if found_open && brace_depth <= 0 {
            return i + 1;
        }

        if i > start && !found_open {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let indent = line.len() - trimmed.len();
                if indent <= start_indent && !trimmed.starts_with('#') && !trimmed.starts_with("//")
                {
                    return i;
                }
            }
        }
    }

    next_boundary
}

fn fallback_chunk(lines: &[&str], max_lines: usize) -> Vec<CodeChunk> {
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < lines.len() {
        let end = (start + max_lines).min(lines.len());
        let content = lines[start..end].join("\n");
        if !content.trim().is_empty() {
            chunks.push(CodeChunk {
                content,
                start_line: start + 1,
                end_line: end,
                kind: ChunkKind::Other,
            });
        }
        start = end;
    }

    chunks
}

pub fn chunk_code_to_strings(source: &str, file_path: &Path, max_lines: usize) -> Vec<String> {
    chunk_code(source, file_path, max_lines)
        .into_iter()
        .map(|c| c.content)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_boundaries() {
        let code = "\
use std::io;

fn helper() -> i32 {
    42
}

pub fn main() {
    let x = helper();
    println!(\"{x}\");
}";
        let chunks = chunk_code(code, Path::new("src/main.rs"), 50);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().any(|c| c.content.contains("fn helper")));
        assert!(chunks.iter().any(|c| c.content.contains("pub fn main")));
    }

    #[test]
    fn python_class_and_def() {
        let code = "\
import os

class Foo:
    def __init__(self):
        self.x = 1

    def bar(self):
        return self.x

def standalone():
    pass
";
        let chunks = chunk_code(code, Path::new("app.py"), 50);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn typescript_functions() {
        let code = "\
export function greet(name: string): string {
  return `Hello, ${name}`;
}

export const add = (a: number, b: number) => a + b;
";
        let chunks = chunk_code(code, Path::new("utils.ts"), 50);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|c| c.content.contains("greet")));
    }

    #[test]
    fn fallback_for_unknown_language() {
        let code = "line1\nline2\nline3\nline4\nline5\n";
        let chunks = chunk_code(code, Path::new("data.txt"), 3);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn empty_source() {
        let chunks = chunk_code("", Path::new("empty.rs"), 50);
        assert!(chunks.is_empty());
    }

    #[test]
    fn go_func_boundaries() {
        let code = "\
package main

func add(a, b int) int {
    return a + b
}

func main() {
    fmt.Println(add(1, 2))
}
";
        let chunks = chunk_code(code, Path::new("main.go"), 50);
        assert!(chunks.len() >= 2);
    }
}
