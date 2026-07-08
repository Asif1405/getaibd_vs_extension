use std::path::Path;

use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub content: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: ChunkKind,
    /// Symbol name (function/class/etc.) when it could be resolved from the AST.
    /// `None` for preamble/fallback chunks or unparsed languages.
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkKind {
    Function,
    Class,
    Impl,
    Module,
    Other,
}

impl ChunkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Class => "type",
            Self::Impl => "impl",
            Self::Module => "mod",
            Self::Other => "code",
        }
    }
}

/// Split `source` into semantic chunks. Uses tree-sitter to cut on real
/// function/class/impl boundaries (capturing the symbol name) for supported
/// languages, and falls back to fixed line windows for everything else or when
/// parsing yields nothing.
pub fn chunk_code(source: &str, file_path: &Path, max_lines: usize) -> Vec<CodeChunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    if let Some(lang) = detect_language(file_path) {
        let chunks = ast_chunk(source, &lines, lang, max_lines);
        if !chunks.is_empty() {
            return chunks;
        }
    }

    fallback_chunk(&lines, max_lines)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language_ {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
}

fn detect_language(path: &Path) -> Option<Language_> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(Language_::Rust),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => Some(Language_::TypeScript),
        Some("py" | "pyi") => Some(Language_::Python),
        Some("go") => Some(Language_::Go),
        Some("java") => Some(Language_::Java),
        _ => None,
    }
}

fn ts_language(path_lang: Language_, is_tsx: bool) -> Language {
    match path_lang {
        Language_::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language_::Python => tree_sitter_python::LANGUAGE.into(),
        Language_::Go => tree_sitter_go::LANGUAGE.into(),
        Language_::Java => tree_sitter_java::LANGUAGE.into(),
        Language_::TypeScript => {
            if is_tsx {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
    }
}

/// How a node kind should be treated during the walk.
enum NodeClass {
    /// A complete unit (function/struct/etc.) — emit whole, don't recurse.
    Leaf(ChunkKind),
    /// A container (class/impl/mod) — emit whole if small, else recurse into it
    /// so its members become individual chunks.
    Container(ChunkKind),
    /// A `const x = () => ...` style binding — emit only if it wraps a function.
    MaybeFunc,
    /// Transparent — descend into children looking for definitions.
    Descend,
}

fn classify(lang: Language_, kind: &str) -> NodeClass {
    use ChunkKind::{Class, Function, Impl, Module, Other};
    match lang {
        Language_::Rust => match kind {
            "function_item" => NodeClass::Leaf(Function),
            "struct_item" | "enum_item" | "union_item" => NodeClass::Leaf(Class),
            "macro_definition" | "macro_rules!" => NodeClass::Leaf(Other),
            "impl_item" => NodeClass::Container(Impl),
            "trait_item" => NodeClass::Container(Class),
            "mod_item" => NodeClass::Container(Module),
            _ => NodeClass::Descend,
        },
        Language_::Python => match kind {
            "function_definition" => NodeClass::Leaf(Function),
            "class_definition" => NodeClass::Container(Class),
            _ => NodeClass::Descend,
        },
        Language_::TypeScript => match kind {
            "function_declaration" | "generator_function_declaration"
            | "function_signature" => NodeClass::Leaf(Function),
            "method_definition" | "method_signature" => NodeClass::Leaf(Function),
            "interface_declaration" | "enum_declaration" | "type_alias_declaration" => {
                NodeClass::Leaf(Class)
            }
            "class_declaration" | "abstract_class_declaration" => NodeClass::Container(Class),
            "lexical_declaration" | "variable_declaration" => NodeClass::MaybeFunc,
            _ => NodeClass::Descend,
        },
        Language_::Go => match kind {
            "function_declaration" | "method_declaration" => NodeClass::Leaf(Function),
            "type_declaration" => NodeClass::Leaf(Class),
            _ => NodeClass::Descend,
        },
        Language_::Java => match kind {
            "method_declaration" | "constructor_declaration" => NodeClass::Leaf(Function),
            "record_declaration" => NodeClass::Leaf(Class),
            "class_declaration" | "interface_declaration" | "enum_declaration" => {
                NodeClass::Container(Class)
            }
            _ => NodeClass::Descend,
        },
    }
}

fn ast_chunk(source: &str, lines: &[&str], lang: Language_, max_lines: usize) -> Vec<CodeChunk> {
    let is_tsx = false; // grammar handles both; TSX superset is safe for TS too
    let language = ts_language(lang, is_tsx);
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut chunks = Vec::new();
    let root = tree.root_node();
    collect(root, source, lines, lang, max_lines, &mut chunks);

    if chunks.is_empty() {
        return Vec::new();
    }

    chunks.sort_by_key(|c| c.start_line);

    // Preamble (imports / top-of-file) before the first definition.
    let first = chunks[0].start_line;
    if first > 3 {
        let preamble = lines[..first - 1].join("\n");
        if !preamble.trim().is_empty() {
            chunks.insert(
                0,
                CodeChunk {
                    content: preamble,
                    start_line: 1,
                    end_line: first - 1,
                    kind: ChunkKind::Other,
                    symbol: None,
                },
            );
        }
    }

    chunks
}

fn collect(
    node: Node,
    source: &str,
    lines: &[&str],
    lang: Language_,
    max_lines: usize,
    out: &mut Vec<CodeChunk>,
) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        match classify(lang, child.kind()) {
            NodeClass::Leaf(kind) => emit(child, source, lines, lang, max_lines, kind, out),
            NodeClass::Container(kind) => {
                let span = child.end_position().row - child.start_position().row + 1;
                if span <= max_lines {
                    emit(child, source, lines, lang, max_lines, kind, out);
                } else {
                    let before = out.len();
                    collect(child, source, lines, lang, max_lines, out);
                    // No members surfaced — keep the (truncated) container itself.
                    if out.len() == before {
                        emit(child, source, lines, lang, max_lines, kind, out);
                    }
                }
            }
            NodeClass::MaybeFunc => {
                let text = child.utf8_text(source.as_bytes()).unwrap_or("");
                if text.contains("=>") || text.contains("function") {
                    emit(
                        child,
                        source,
                        lines,
                        lang,
                        max_lines,
                        ChunkKind::Function,
                        out,
                    );
                }
            }
            NodeClass::Descend => collect(child, source, lines, lang, max_lines, out),
        }
    }
}

fn emit(
    node: Node,
    source: &str,
    lines: &[&str],
    lang: Language_,
    max_lines: usize,
    kind: ChunkKind,
    out: &mut Vec<CodeChunk>,
) {
    let start_row = node.start_position().row;
    let end_row = node.end_position().row;
    if start_row >= lines.len() {
        return;
    }
    let last = end_row.min(start_row + max_lines - 1).min(lines.len() - 1);
    let content = lines[start_row..=last].join("\n");
    if content.trim().is_empty() {
        return;
    }
    out.push(CodeChunk {
        content,
        start_line: start_row + 1,
        end_line: last + 1,
        kind,
        symbol: symbol_of(node, source, lang),
    });
}

/// Best-effort symbol name for a definition node.
fn symbol_of(node: Node, source: &str, lang: Language_) -> Option<String> {
    let bytes = source.as_bytes();

    // Rust `impl` blocks have no `name`; use the type they implement for.
    if lang == Language_::Rust && node.kind() == "impl_item" {
        if let Some(t) = node.child_by_field_name("type") {
            return t.utf8_text(bytes).ok().map(clean_symbol);
        }
    }

    if let Some(name) = node.child_by_field_name("name") {
        return name.utf8_text(bytes).ok().map(clean_symbol);
    }

    // `const foo = () => {}` — dig into the declarator.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name) = child.child_by_field_name("name") {
                return name.utf8_text(bytes).ok().map(clean_symbol);
            }
        }
        // Go `type X struct {}` — type_spec holds the name.
        if child.kind() == "type_spec" {
            if let Some(name) = child.child_by_field_name("name") {
                return name.utf8_text(bytes).ok().map(clean_symbol);
            }
        }
    }

    // Last resort: first identifier-ish named child.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
        ) {
            return child.utf8_text(bytes).ok().map(clean_symbol);
        }
    }

    None
}

fn clean_symbol(s: &str) -> String {
    let one_line = s.split('\n').next().unwrap_or(s).trim();
    let capped: String = one_line.chars().take(80).collect();
    capped
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
                symbol: None,
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

/// A named definition in a file, for an outline / `document_symbols` fallback.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

/// Best-effort symbol outline for a file, built from the AST chunker. Empty for
/// languages tree-sitter can't parse here.
pub fn symbol_outline(source: &str, file_path: &Path) -> Vec<SymbolInfo> {
    // A large window keeps whole top-level definitions intact so we list them once.
    chunk_code(source, file_path, 100_000)
        .into_iter()
        .filter_map(|c| {
            c.symbol.map(|name| SymbolInfo {
                name,
                kind: c.kind.as_str().to_string(),
                line: c.start_line,
            })
        })
        .collect()
}

/// A definition with its full (uncapped) line span, including nested methods.
#[derive(Debug, Clone)]
pub struct SymbolRange {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
}

impl SymbolRange {
    /// Whether this symbol's span overlaps the 1-based inclusive line range.
    pub fn overlaps(&self, start: usize, end: usize) -> bool {
        self.start_line <= end && self.end_line >= start
    }

    pub fn span(&self) -> usize {
        self.end_line.saturating_sub(self.start_line)
    }
}

/// Every named definition in a file with its full span (containers AND their
/// members), for mapping diff hunks to the exact symbol they touch. Empty for
/// languages tree-sitter can't parse here.
pub fn symbol_ranges(source: &str, file_path: &Path) -> Vec<SymbolRange> {
    let Some(lang) = detect_language(file_path) else {
        return Vec::new();
    };
    let language = ts_language(lang, false);
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = match classify(lang, node.kind()) {
            NodeClass::Leaf(k) | NodeClass::Container(k) => Some(k),
            NodeClass::MaybeFunc => {
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                (text.contains("=>") || text.contains("function"))
                    .then_some(ChunkKind::Function)
            }
            NodeClass::Descend => None,
        };
        if let Some(kind) = kind {
            if let Some(name) = symbol_of(node, source, lang) {
                out.push(SymbolRange {
                    name,
                    kind: kind.as_str().to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                });
            }
        }
        // Always descend so members of a container are recorded too.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// Tree-sitter syntax errors for a file (parse errors / missing nodes), used as a
/// headless post-edit sanity check. Returns `(line, message)` pairs, empty when the
/// language is unsupported here or the file parses cleanly.
pub fn syntax_errors(source: &str, file_path: &Path) -> Vec<(usize, String)> {
    let Some(lang) = detect_language(file_path) else {
        return Vec::new();
    };
    let language = ts_language(lang, false);
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    if !tree.root_node().has_error() {
        return Vec::new();
    }

    let mut errors = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() {
            errors.push((
                node.start_position().row + 1,
                "syntax error".to_string(),
            ));
        } else if node.is_missing() {
            errors.push((
                node.start_position().row + 1,
                format!("missing {}", node.kind()),
            ));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    errors.sort_by_key(|(line, _)| *line);
    errors.dedup();
    errors
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
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("helper")));
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("main")));
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
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("standalone")));
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
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("greet")));
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("add")));
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
        assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("add")));
    }

    #[test]
    fn large_class_splits_into_methods() {
        // A class larger than max_lines should surface its methods as chunks.
        let mut code = String::from("class Big:\n");
        for i in 0..5 {
            code.push_str(&format!("    def method{i}(self):\n"));
            for _ in 0..5 {
                code.push_str("        pass\n");
            }
        }
        let chunks = chunk_code(&code, Path::new("big.py"), 10);
        assert!(chunks.iter().filter(|c| c.kind == ChunkKind::Function).count() >= 2);
    }
}
