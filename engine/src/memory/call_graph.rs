use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use syn::visit::{self, Visit};
use syn::{Expr, ExprCall, ExprMethodCall, ItemEnum, ItemFn, ItemImpl, ItemStruct, ItemTrait};

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct CallGraph {
    pub nodes: HashMap<String, FunctionNode>,
    pub edges: Vec<CallEdge>,
}

#[derive(Debug, Clone)]
pub struct FunctionNode {
    pub name: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub is_public: bool,
    pub is_async: bool,
    pub params: Vec<String>,
    pub return_type: Option<String>,
    pub doc_comment: Option<String>,
    pub calls: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
    pub file_path: String,
}

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub name: String,
    pub file_path: String,
    pub kind: TypeKind,
    pub fields: Vec<String>,
    pub methods: Vec<String>,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeKind {
    Struct,
    Enum,
    Trait,
}

pub struct CallGraphBuilder {
    graph: CallGraph,
    types: HashMap<String, TypeInfo>,
    current_file: String,
    current_function: Option<String>,
}

impl CallGraphBuilder {
    pub fn new() -> Self {
        Self {
            graph: CallGraph {
                nodes: HashMap::new(),
                edges: Vec::new(),
            },
            types: HashMap::new(),
            current_file: String::new(),
            current_function: None,
        }
    }

    pub fn analyze_file(&mut self, file_path: &Path) -> Result<(), AppError> {
        let content = fs::read_to_string(file_path)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to read file: {}", e)))?;

        self.current_file = file_path.to_string_lossy().to_string();

        let syntax = syn::parse_file(&content)
            .map_err(|e| AppError::InvalidRequest(format!("Failed to parse Rust file: {}", e)))?;

        self.visit_file(&syntax);

        Ok(())
    }

    pub fn build(self) -> (CallGraph, HashMap<String, TypeInfo>) {
        (self.graph, self.types)
    }
}

impl<'ast> Visit<'ast> for CallGraphBuilder {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let fn_name = node.sig.ident.to_string();
        let is_public = matches!(node.vis, syn::Visibility::Public(_));
        let is_async = node.sig.asyncness.is_some();

        let params: Vec<String> = node
            .sig
            .inputs
            .iter()
            .filter_map(|arg| {
                if let syn::FnArg::Typed(pat_type) = arg {
                    if let syn::Pat::Ident(ident) = &*pat_type.pat {
                        return Some(ident.ident.to_string());
                    }
                }
                None
            })
            .collect();

        let return_type = match &node.sig.output {
            syn::ReturnType::Type(_, ty) => Some(quote::quote!(#ty).to_string()),
            syn::ReturnType::Default => None,
        };

        let doc_comment = extract_doc_comment(&node.attrs);

        let function_node = FunctionNode {
            name: fn_name.clone(),
            file_path: self.current_file.clone(),
            line_start: 0,
            line_end: 0,
            is_public,
            is_async,
            params,
            return_type,
            doc_comment,
            calls: Vec::new(),
        };

        self.current_function = Some(fn_name.clone());
        self.graph.nodes.insert(fn_name, function_node);

        visit::visit_item_fn(self, node);

        self.current_function = None;
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Some(current_fn) = &self.current_function {
            if let Expr::Path(expr_path) = &*node.func {
                if let Some(segment) = expr_path.path.segments.last() {
                    let callee = segment.ident.to_string();

                    if let Some(fn_node) = self.graph.nodes.get_mut(current_fn) {
                        fn_node.calls.push(callee.clone());
                    }

                    self.graph.edges.push(CallEdge {
                        caller: current_fn.clone(),
                        callee,
                        file_path: self.current_file.clone(),
                    });
                }
            }
        }

        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if let Some(current_fn) = &self.current_function {
            let method_name = node.method.to_string();

            if let Some(fn_node) = self.graph.nodes.get_mut(current_fn) {
                fn_node.calls.push(method_name.clone());
            }

            self.graph.edges.push(CallEdge {
                caller: current_fn.clone(),
                callee: method_name,
                file_path: self.current_file.clone(),
            });
        }

        visit::visit_expr_method_call(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let struct_name = node.ident.to_string();
        let is_public = matches!(node.vis, syn::Visibility::Public(_));

        if !is_public {
            visit::visit_item_struct(self, node);
            return;
        }

        let fields: Vec<String> = match &node.fields {
            syn::Fields::Named(fields) => fields
                .named
                .iter()
                .filter_map(|f| f.ident.as_ref().map(|i| i.to_string()))
                .collect(),
            syn::Fields::Unnamed(fields) => (0..fields.unnamed.len())
                .map(|i| format!("field_{}", i))
                .collect(),
            syn::Fields::Unit => Vec::new(),
        };

        let doc_comment = extract_doc_comment(&node.attrs);

        self.types.insert(
            struct_name.clone(),
            TypeInfo {
                name: struct_name,
                file_path: self.current_file.clone(),
                kind: TypeKind::Struct,
                fields,
                methods: Vec::new(),
                doc_comment,
            },
        );

        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        let enum_name = node.ident.to_string();
        let is_public = matches!(node.vis, syn::Visibility::Public(_));

        if !is_public {
            visit::visit_item_enum(self, node);
            return;
        }

        let variants: Vec<String> = node.variants.iter().map(|v| v.ident.to_string()).collect();

        let doc_comment = extract_doc_comment(&node.attrs);

        self.types.insert(
            enum_name.clone(),
            TypeInfo {
                name: enum_name,
                file_path: self.current_file.clone(),
                kind: TypeKind::Enum,
                fields: variants,
                methods: Vec::new(),
                doc_comment,
            },
        );

        visit::visit_item_enum(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast ItemTrait) {
        let trait_name = node.ident.to_string();
        let is_public = matches!(node.vis, syn::Visibility::Public(_));

        if !is_public {
            visit::visit_item_trait(self, node);
            return;
        }

        let methods: Vec<String> = node
            .items
            .iter()
            .filter_map(|item| {
                if let syn::TraitItem::Fn(method) = item {
                    Some(method.sig.ident.to_string())
                } else {
                    None
                }
            })
            .collect();

        let doc_comment = extract_doc_comment(&node.attrs);

        self.types.insert(
            trait_name.clone(),
            TypeInfo {
                name: trait_name,
                file_path: self.current_file.clone(),
                kind: TypeKind::Trait,
                fields: Vec::new(),
                methods,
                doc_comment,
            },
        );

        visit::visit_item_trait(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let type_name = if let syn::Type::Path(type_path) = &*node.self_ty {
            type_path.path.segments.last().map(|s| s.ident.to_string())
        } else {
            None
        };

        if let Some(type_name) = type_name {
            let methods: Vec<String> = node
                .items
                .iter()
                .filter_map(|item| {
                    if let syn::ImplItem::Fn(method) = item {
                        Some(method.sig.ident.to_string())
                    } else {
                        None
                    }
                })
                .collect();

            if let Some(type_info) = self.types.get_mut(&type_name) {
                type_info.methods.extend(methods);
            }
        }

        visit::visit_item_impl(self, node);
    }
}

fn extract_doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    let mut doc_lines = Vec::new();

    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let syn::Meta::NameValue(meta) = &attr.meta {
                if let syn::Expr::Lit(expr_lit) = &meta.value {
                    if let syn::Lit::Str(lit_str) = &expr_lit.lit {
                        doc_lines.push(lit_str.value().trim().to_string());
                    }
                }
            }
        }
    }

    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join("\n"))
    }
}

impl CallGraph {
    pub fn find_callers(&self, function_name: &str) -> Vec<String> {
        self.edges
            .iter()
            .filter(|e| e.callee == function_name)
            .map(|e| e.caller.clone())
            .collect()
    }

    pub fn find_callees(&self, function_name: &str) -> Vec<String> {
        self.nodes
            .get(function_name)
            .map(|node| node.calls.clone())
            .unwrap_or_default()
    }

    pub fn find_call_chain(&self, from: &str, to: &str, max_depth: usize) -> Option<Vec<String>> {
        let mut visited = HashSet::new();
        let mut path = vec![from.to_string()];

        if self.dfs_find_path(from, to, &mut visited, &mut path, 0, max_depth) {
            Some(path)
        } else {
            None
        }
    }

    fn dfs_find_path(
        &self,
        current: &str,
        target: &str,
        visited: &mut HashSet<String>,
        path: &mut Vec<String>,
        depth: usize,
        max_depth: usize,
    ) -> bool {
        if current == target {
            return true;
        }

        if depth >= max_depth || visited.contains(current) {
            return false;
        }

        visited.insert(current.to_string());

        let callees = self.find_callees(current);
        for callee in callees {
            path.push(callee.clone());
            if self.dfs_find_path(&callee, target, visited, path, depth + 1, max_depth) {
                return true;
            }
            path.pop();
        }

        false
    }

    pub fn get_function_complexity(&self, function_name: &str) -> usize {
        self.nodes
            .get(function_name)
            .map(|node| node.calls.len())
            .unwrap_or(0)
    }
}
