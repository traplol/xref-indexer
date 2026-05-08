use std::collections::HashSet;
use std::path::Path;

use tree_sitter::{Language, Node, Parser as TsParser, Point};

use crate::parser::Parser;
use crate::types::*;

/// C/C++ parser using tree-sitter.
pub struct CppParser {
    ts_language: Language,
    language_name: &'static str,
}

impl CppParser {
    pub fn new_c() -> Self {
        Self {
            ts_language: tree_sitter_c::LANGUAGE.into(),
            language_name: "c",
        }
    }

    pub fn new_cpp() -> Self {
        Self {
            ts_language: tree_sitter_cpp::LANGUAGE.into(),
            language_name: "cpp",
        }
    }
}

impl Parser for CppParser {
    fn parse_file(&self, path: &Path, source: &str) -> Result<FileSymbols, String> {
        let mut parser = TsParser::new();
        parser
            .set_language(&self.ts_language)
            .map_err(|e| format!("failed to set language: {e}"))?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| "parse returned None".to_string())?;

        let mut state = ParseState::new(path, source);

        let root = tree.root_node();
        // Walk first to collect definitions and their name locations.
        state.collect_definitions = true;
        state.walk_node(root, self.language_name);

        // Second pass: collect references at identifier nodes not in def_locations.
        state.collect_definitions = false;
        state.walk_node(root, self.language_name);

        Ok(FileSymbols {
            file: path.to_path_buf(),
            language: self.language_name.to_string(),
            definitions: state.definitions,
            references: state.references,
            calls: state.calls,
            inherits: state.inherits,
        })
    }

    fn language(&self) -> &'static str {
        self.language_name
    }
}

struct ParseState<'a> {
    source: &'a str,
    file: &'a Path,
    collect_definitions: bool,
    def_locations: HashSet<(usize, usize)>,
    handled_ref_locations: HashSet<(usize, usize)>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    calls: Vec<CallEdge>,
    inherits: Vec<InheritEdge>,
    scope_stack: Vec<(String, SymbolKind)>,
    visibility_stack: Vec<Visibility>,
    current_function: Option<String>,
    line_starts: Vec<usize>,
}

impl<'a> ParseState<'a> {
    fn new(file: &'a Path, source: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self {
            source,
            file,
            collect_definitions: true,
            def_locations: HashSet::new(),
            handled_ref_locations: HashSet::new(),
            definitions: Vec::new(),
            references: Vec::new(),
            calls: Vec::new(),
            inherits: Vec::new(),
            scope_stack: Vec::new(),
            visibility_stack: Vec::new(),
            current_function: None,
            line_starts,
        }
    }

    fn current_visibility(&self) -> Visibility {
        self.visibility_stack
            .last()
            .copied()
            .unwrap_or(Visibility::Public)
    }

    fn qualified_name(&self, name: &str) -> String {
        self.current_scope_qualified_name()
            .map(|scope| format!("{scope}::{name}"))
            .unwrap_or_else(|| name.to_string())
    }

    fn push_scope(&mut self, name: &str, kind: SymbolKind, vis: Visibility) {
        self.scope_stack.push((name.to_string(), kind));
        self.visibility_stack.push(vis);
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
        self.visibility_stack.pop();
    }

    fn current_scope_qualified_name(&self) -> Option<String> {
        let scopes: Vec<&str> = self
            .scope_stack
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        if scopes.is_empty() {
            None
        } else {
            Some(scopes.join("::"))
        }
    }

    fn parent_qualified_name(&self) -> Option<String> {
        self.current_scope_qualified_name()
    }

    fn qualified_parent(&self, qualifier: Option<&str>) -> Option<String> {
        match (self.current_scope_qualified_name(), qualifier) {
            (Some(scope), Some(q)) if !q.is_empty() => Some(format!("{scope}::{q}")),
            (None, Some(q)) if !q.is_empty() => Some(q.to_string()),
            (Some(scope), _) => Some(scope),
            _ => None,
        }
    }

    fn is_in_type_scope(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|(_, k)| matches!(k, SymbolKind::Class | SymbolKind::Struct))
    }

    fn point_to_location(&self, start: Point, end: Point) -> Location {
        Location {
            file: self.file.to_path_buf(),
            line: start.row + 1,
            column: start.column,
            end_line: end.row + 1,
            end_column: end.column,
        }
    }

    fn node_text(&self, node: Node) -> &str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn line_context(&self, line: usize) -> Option<String> {
        if line == 0 || line > self.line_starts.len() {
            return None;
        }
        let start = self.line_starts[line - 1];
        let end = match self.line_starts.get(line) {
            Some(&e) => {
                if e > 0 && self.source.as_bytes()[e - 1] == b'\n' {
                    e - 1
                } else {
                    e
                }
            }
            None => self.source.len(),
        };
        if start < self.source.len() {
            Some(self.source[start..end].to_string())
        } else {
            None
        }
    }

    /// Walk the tree recursively.
    fn walk_node(&mut self, node: Node<'a>, language: &str) {
        let kind = node.kind();

        match kind {
            // ── Scope-introducing nodes ──
            "namespace_definition" => {
                if self.collect_definitions {
                    if let Some(name) = extract_namespace_name(node, self.source) {
                        self.register_def(&name, node, SymbolKind::Namespace, false);
                    }
                }
                // Even if no name (anonymous namespace), push a scope.
                let name = extract_namespace_name(node, self.source).unwrap_or_default();
                self.push_scope(&name, SymbolKind::Namespace, Visibility::Public);
                self.walk_children(node, language);
                self.pop_scope();
                return;
            }

            "class_specifier" | "struct_specifier" | "union_specifier" => {
                if self.collect_definitions {
                    if let Some(name) = extract_typename(node, self.source) {
                        let sk = match kind {
                            "class_specifier" => SymbolKind::Class,
                            "struct_specifier" => SymbolKind::Struct,
                            _ => SymbolKind::Struct,
                        };
                        self.register_def(&name, node, sk, false);
                    }
                }
                let name = extract_typename(node, self.source).unwrap_or_default();
                // Default visibility: class = private, struct/union = public.
                let default_vis = match kind {
                    "class_specifier" => Visibility::Private,
                    _ => Visibility::Public,
                };
                self.push_scope(&name, SymbolKind::Class, default_vis);
                self.walk_children(node, language);
                self.pop_scope();
                return;
            }

            "enum_specifier" => {
                if self.collect_definitions {
                    if let Some(name) = extract_typename(node, self.source) {
                        self.register_def(&name, node, SymbolKind::Enum, false);
                    }
                }
                let name = extract_typename(node, self.source).unwrap_or_default();
                self.push_scope(&name, SymbolKind::Enum, Visibility::Public);
                self.walk_children(node, language);
                self.pop_scope();
                return;
            }

            "function_definition" => {
                let prev_function = self.current_function.clone();
                if let Some((qualifier, name)) = extract_function_name(node, self.source) {
                    let parent = self.qualified_parent(qualifier.as_deref());
                    let qn = parent
                        .as_ref()
                        .map(|p| format!("{p}::{name}"))
                        .unwrap_or_else(|| name.clone());
                    self.current_function = Some(qn.clone());

                    if self.collect_definitions {
                        let is_method = self.is_in_type_scope() || qualifier.is_some();
                        let kind = classify_function(&name, parent.as_deref(), is_method);
                        let sig = child_of_kind(node, "function_declarator").and_then(|func_dec| {
                            self.source
                                .get(node.start_byte()..func_dec.end_byte())
                                .map(|s| s.trim().to_string())
                        });
                        let vis = self.current_visibility();

                        let name_node = child_of_kind(node, "function_declarator")
                            .and_then(find_declarator_name_node)
                            .or_else(|| find_name_node(node, self.source));
                        let name_loc = name_node
                            .map(|n| self.point_to_location(n.start_position(), n.end_position()))
                            .unwrap_or_else(|| {
                                self.point_to_location(node.start_position(), node.end_position())
                            });

                        self.def_locations.insert((name_loc.line, name_loc.column));
                        self.definitions.push(Definition {
                            id: None,
                            name: name.clone(),
                            qualified_name: qn.clone(),
                            kind,
                            location: name_loc,
                            parent,
                            signature: sig,
                            visibility: vis,
                            is_definition: true,
                            extra: None,
                        });
                    }
                }
                self.walk_children(node, language);
                self.current_function = prev_function;
                return;
            }

            "declaration" => {
                if self.collect_definitions {
                    self.handle_declaration(node, language);
                }
                self.walk_children(node, language);
                return;
            }

            "field_declaration" => {
                if self.collect_definitions {
                    self.handle_declaration(node, language);
                }
                self.walk_children(node, language);
                return;
            }

            "preproc_def" => {
                if self.collect_definitions {
                    self.handle_macro_def(node);
                }
                self.walk_children(node, language);
                return;
            }

            "preproc_function_def" => {
                if self.collect_definitions {
                    if let Some(name) = node.child_by_field_name("name") {
                        let loc =
                            self.point_to_location(name.start_position(), name.end_position());
                        self.def_locations.insert((loc.line, loc.column));
                        let qn = self.qualified_name(&self.node_text(name));
                        self.definitions.push(Definition {
                            id: None,
                            name: self.node_text(name).to_string(),
                            qualified_name: qn,
                            kind: SymbolKind::Macro,
                            location: loc,
                            parent: None,
                            signature: None,
                            visibility: Visibility::Public,
                            is_definition: true,
                            extra: None,
                        });
                    }
                }
                self.walk_children(node, language);
                return;
            }

            "type_definition" => {
                if self.collect_definitions {
                    self.handle_typedef(node);
                }
                self.walk_children(node, language);
                return;
            }

            "enumerator" => {
                if self.collect_definitions {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let name = self.node_text(name_node).to_string();
                        let loc = self.point_to_location(
                            name_node.start_position(),
                            name_node.end_position(),
                        );
                        self.def_locations.insert((loc.line, loc.column));
                        let qn = self.qualified_name(&name);
                        self.definitions.push(Definition {
                            id: None,
                            name,
                            qualified_name: qn,
                            kind: SymbolKind::EnumVariant,
                            location: loc,
                            parent: self.parent_qualified_name(),
                            signature: None,
                            visibility: Visibility::Public,
                            is_definition: true,
                            extra: None,
                        });
                    }
                }
                self.walk_children(node, language);
                return;
            }

            // ── Template wrappers ──
            "template_declaration" | "template_specification" => {
                // Walk children — the real declaration is nested inside.
                self.walk_children(node, language);
                return;
            }

            // ── Inheritance (C++ only) ──
            "base_class_clause" => {
                if self.collect_definitions {
                    self.handle_inheritance(node);
                }
                self.walk_children(node, language);
                return;
            }

            // ── Access specifiers (C++ only) ──
            "access_specifier" => {
                if let Some(vis) = extract_access(node, self.source) {
                    if let Some(last) = self.visibility_stack.last_mut() {
                        *last = vis;
                    }
                }
                self.walk_children(node, language);
                return;
            }

            // ── References (second pass) ──
            "identifier" | "type_identifier" | "field_identifier" | "namespace_identifier" => {
                if !self.collect_definitions {
                    self.handle_reference(node);
                }
                // Don't recurse — identifiers are leaves.
                return;
            }

            // ── Call expressions ──
            "call_expression" => {
                if !self.collect_definitions {
                    self.handle_call(node);
                }
                self.walk_children(node, language);
                return;
            }

            _ => {}
        }

        self.walk_children(node, language);
    }

    fn walk_children(&mut self, node: Node<'a>, language: &str) {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.walk_node(child, language);
            }
        }
    }

    fn register_def(&mut self, name: &str, node: Node, kind: SymbolKind, is_definition: bool) {
        let name_node = find_name_node(node, self.source).unwrap_or(node);
        let loc = self.point_to_location(name_node.start_position(), name_node.end_position());
        self.def_locations.insert((loc.line, loc.column));
        let qn = self.qualified_name(name);
        self.definitions.push(Definition {
            id: None,
            name: name.to_string(),
            qualified_name: qn,
            kind,
            location: loc,
            parent: self.parent_qualified_name(),
            signature: None,
            visibility: self.current_visibility(),
            is_definition,
            extra: None,
        });
    }

    fn handle_declaration(&mut self, node: Node, _language: &str) {
        // Check if this is a function forward-declaration.
        if let Some(func_dec) = child_of_kind(node, "function_declarator") {
            if let Some((qualifier, name)) = extract_name_from_declarator(func_dec, self.source) {
                let parent = self.qualified_parent(qualifier.as_deref());
                let qn = parent
                    .as_ref()
                    .map(|p| format!("{p}::{name}"))
                    .unwrap_or_else(|| name.clone());
                let is_method = self.is_in_type_scope() || qualifier.is_some();
                let kind = classify_function(&name, parent.as_deref(), is_method);

                let name_node = find_declarator_name_node(func_dec)
                    .or_else(|| find_name_node(node, self.source))
                    .unwrap_or(node);
                let name_loc =
                    self.point_to_location(name_node.start_position(), name_node.end_position());
                self.def_locations.insert((name_loc.line, name_loc.column));
                self.definitions.push(Definition {
                    id: None,
                    name,
                    qualified_name: qn,
                    kind,
                    location: name_loc,
                    parent,
                    signature: Some(self.node_text(node).to_string()),
                    visibility: self.current_visibility(),
                    is_definition: false,
                    extra: None,
                });
            }
            return;
        }

        // Check for variable/field declarations.
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                let handled = self.try_register_variable(child);
                if handled {
                    return;
                }
            }
        }
    }

    /// Try to register a variable/field from a declarator node.
    /// Returns true if a variable was registered.
    fn try_register_variable(&mut self, declarator: Node) -> bool {
        let name_node = match declarator.kind() {
            "init_declarator"
            | "pointer_declarator"
            | "array_declarator"
            | "reference_declarator" => {
                descendant_of_kind(declarator, &["identifier", "field_identifier"])
            }
            "identifier" | "field_identifier" => Some(declarator),
            _ => None,
        };

        if let Some(name_node) = name_node {
            let name = self.node_text(name_node).to_string();
            let loc = self.point_to_location(name_node.start_position(), name_node.end_position());
            self.def_locations.insert((loc.line, loc.column));
            let qn = self.qualified_name(&name);
            let is_field = self
                .scope_stack
                .last()
                .map(|(_, k)| {
                    matches!(k, SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum)
                })
                .unwrap_or(false);
            let kind = if is_field {
                SymbolKind::Field
            } else {
                SymbolKind::Variable
            };
            self.definitions.push(Definition {
                id: None,
                name,
                qualified_name: qn,
                kind,
                location: loc,
                parent: self.parent_qualified_name(),
                signature: None,
                visibility: self.current_visibility(),
                is_definition: true,
                extra: None,
            });
            return true;
        }
        false
    }

    fn handle_macro_def(&mut self, node: Node) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = self.node_text(name_node).to_string();
            let loc = self.point_to_location(name_node.start_position(), name_node.end_position());
            self.def_locations.insert((loc.line, loc.column));
            self.definitions.push(Definition {
                id: None,
                name,
                qualified_name: self.qualified_name(&self.node_text(name_node)),
                kind: SymbolKind::Macro,
                location: loc,
                parent: None,
                signature: None,
                visibility: Visibility::Public,
                is_definition: true,
                extra: None,
            });
        }
    }

    fn handle_typedef(&mut self, node: Node) {
        // The typedef name is in the last type_identifier child.
        let mut name_node: Option<Node> = None;
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                if child.kind() == "type_identifier" {
                    name_node = Some(child);
                }
            }
        }
        if let Some(name_node) = name_node {
            let name = self.node_text(name_node).to_string();
            let loc = self.point_to_location(name_node.start_position(), name_node.end_position());
            self.def_locations.insert((loc.line, loc.column));
            let qn = self.qualified_name(&name);
            self.definitions.push(Definition {
                id: None,
                name,
                qualified_name: qn,
                kind: SymbolKind::Typedef,
                location: loc,
                parent: self.parent_qualified_name(),
                signature: None,
                visibility: self.current_visibility(),
                is_definition: true,
                extra: None,
            });
        }
    }

    fn handle_reference(&mut self, node: Node) {
        let loc = self.point_to_location(node.start_position(), node.end_position());
        // Skip definition sites and already-handled call callees.
        if self.def_locations.contains(&(loc.line, loc.column))
            || self.handled_ref_locations.contains(&(loc.line, loc.column))
        {
            return;
        }
        let name = self.node_text(node).to_string();
        if name.is_empty() || name.len() > 256 {
            return;
        }
        let context = self.line_context(loc.line);
        self.references.push(Reference {
            id: None,
            name,
            kind: SymbolKind::Function,
            location: loc,
            def_id: None,
            context,
        });
    }

    fn handle_call(&mut self, node: Node) {
        let callee_node = node.child(0);
        let caller_name = self.current_function.clone().unwrap_or_default();

        if let Some(callee) = callee_node {
            let callee_name = match callee.kind() {
                "identifier" | "type_identifier" | "field_identifier" => {
                    self.node_text(callee).to_string()
                }
                "field_expression" => callee
                    .child_by_field_name("field")
                    .map(|n| self.node_text(n).to_string())
                    .unwrap_or_default(),
                "qualified_identifier" => {
                    let text = self.node_text(callee);
                    text.split("::").last().unwrap_or(&text).to_string()
                }
                _ => return,
            };

            if callee_name.is_empty() || callee_name == caller_name {
                return;
            }

            let callee_loc = self.point_to_location(callee.start_position(), callee.end_position());
            self.calls.push(CallEdge {
                caller_name: caller_name.clone(),
                callee_name: callee_name.clone(),
                location: callee_loc.clone(),
            });

            let loc_key = (callee_loc.line, callee_loc.column);
            if !self.def_locations.contains(&loc_key)
                && !self.handled_ref_locations.contains(&loc_key)
            {
                let ctx = self.line_context(callee_loc.line);
                self.references.push(Reference {
                    id: None,
                    name: callee_name,
                    kind: SymbolKind::Function,
                    location: callee_loc,
                    def_id: None,
                    context: ctx,
                });
                self.handled_ref_locations.insert(loc_key);
            }
        }
    }

    fn handle_inheritance(&mut self, node: Node) {
        let derived_name = self
            .scope_stack
            .last()
            .map(|(n, _)| n.clone())
            .unwrap_or_default();

        // Default access for base classes: class=private, struct=public.
        let default_access = self.current_visibility();

        let mut current_access = None;
        let mut current_virtual = false;

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                match child.kind() {
                    "access_specifier" => {
                        current_access = extract_access(child, self.source);
                    }
                    "virtual" => {
                        current_virtual = true;
                    }
                    "type_identifier"
                    | "identifier"
                    | "qualified_identifier"
                    | "template_type"
                    | "parameterized_type"
                    | "scoped_type_identifier"
                    | "qualified_type_identifier" => {
                        let base_name = self.node_text(child).to_string();
                        if base_name.is_empty() {
                            continue;
                        }
                        self.inherits.push(InheritEdge {
                            derived_name: derived_name.clone(),
                            base_name,
                            access: current_access.unwrap_or(default_access),
                            is_virtual: current_virtual,
                        });
                        // Reset per-base flags.
                        current_access = None;
                        current_virtual = false;
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── Helper functions ──

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            if child.kind() == kind {
                return Some(child);
            }
        }
    }
    None
}

fn descendant_of_kind<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            if kinds.contains(&child.kind()) {
                return Some(child);
            }
            if let Some(found) = descendant_of_kind(child, kinds) {
                return Some(found);
            }
        }
    }
    None
}

fn classify_function(name: &str, parent: Option<&str>, is_method: bool) -> SymbolKind {
    if name.starts_with('~') {
        return SymbolKind::Destructor;
    }
    if is_method {
        let class_name = parent.and_then(|p| p.rsplit("::").next()).unwrap_or("");
        if name == class_name {
            SymbolKind::Constructor
        } else {
            SymbolKind::Method
        }
    } else {
        SymbolKind::Function
    }
}

fn find_declarator_name_node<'a>(declarator: Node<'a>) -> Option<Node<'a>> {
    let mut current = declarator;
    loop {
        match current.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "destructor_name" => {
                return Some(current);
            }
            "qualified_identifier" => {
                if let Some(name) = current.child_by_field_name("name") {
                    return Some(name);
                }
                let mut result = None;
                for child in current.children(&mut current.walk()) {
                    if matches!(
                        child.kind(),
                        "identifier" | "field_identifier" | "type_identifier" | "destructor_name"
                    ) {
                        result = Some(child);
                    }
                }
                return result;
            }
            "function_declarator"
            | "array_declarator"
            | "pointer_declarator"
            | "reference_declarator" => {
                if let Some(inner) = current.child_by_field_name("declarator") {
                    current = inner;
                } else if let Some(inner) = current.child(0) {
                    current = inner;
                } else {
                    return None;
                }
            }
            _ => {
                if let Some(inner) = current.child_by_field_name("declarator") {
                    current = inner;
                } else if let Some(inner) = current.child_by_field_name("name") {
                    current = inner;
                } else {
                    return None;
                }
            }
        }
    }
}

/// Walk declarator chain to find the name. Returns `(qualifier, name)` where
/// `qualifier` is the scope prefix for out-of-class definitions like `Derived::run`.
fn extract_name_from_declarator(
    declarator: Node,
    source: &str,
) -> Option<(Option<String>, String)> {
    let mut current = declarator;
    loop {
        match current.kind() {
            "identifier" | "field_identifier" | "type_identifier" => {
                let name = current
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .to_string();
                return Some((None, name));
            }
            "qualified_identifier" => {
                let text = current.utf8_text(source.as_bytes()).unwrap_or("");
                if let Some((qual, name)) = text.rsplit_once("::") {
                    if !name.is_empty() {
                        return Some((Some(qual.to_string()), name.to_string()));
                    }
                }
                return None;
            }
            "function_declarator"
            | "array_declarator"
            | "pointer_declarator"
            | "reference_declarator" => {
                if let Some(inner) = current.child_by_field_name("declarator") {
                    current = inner;
                } else if let Some(inner) = current.child(0) {
                    current = inner;
                } else {
                    return None;
                }
            }
            "destructor_name" => {
                let text = current.utf8_text(source.as_bytes()).unwrap_or("");
                return Some((None, text.to_string()));
            }
            _ => {
                if let Some(inner) = current.child_by_field_name("declarator") {
                    current = inner;
                } else if let Some(inner) = current.child_by_field_name("name") {
                    current = inner;
                } else if let Some(inner) = current.child(0) {
                    current = inner;
                } else {
                    return None;
                }
            }
        }
    }
}

fn extract_function_name(node: Node, source: &str) -> Option<(Option<String>, String)> {
    if let Some(func_dec) = child_of_kind(node, "function_declarator") {
        return extract_name_from_declarator(func_dec, source);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            if let Some(result) = extract_name_from_declarator(child, source) {
                let ref_name = &result.1;
                if *ref_name != node.utf8_text(source.as_bytes()).unwrap_or("") {
                    return Some(result);
                }
            }
        }
    }
    None
}

fn extract_typename(node: Node, source: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = name_node.utf8_text(source.as_bytes()).unwrap_or("");
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    // Fallback: look for type_identifier or identifier child.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "type_identifier" | "identifier" => {
                    return Some(child.utf8_text(source.as_bytes()).unwrap_or("").to_string());
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_namespace_name(node: Node, source: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = name_node.utf8_text(source.as_bytes()).unwrap_or("");
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    // Nested namespace: namespace a::b::c { }
    for child in node.children(&mut node.walk()) {
        if child.kind() == "nested_namespace_specifier" || child.kind() == "qualified_identifier" {
            return Some(child.utf8_text(source.as_bytes()).unwrap_or("").to_string());
        }
    }
    None
}

fn find_name_node<'a>(node: Node<'a>, _source: &str) -> Option<Node<'a>> {
    // Try the "name" field first.
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(name_node);
    }
    // Otherwise search for the first identifier-like child.
    descendant_of_kind(
        node,
        &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "namespace_identifier",
            "destructor_name",
        ],
    )
}

fn extract_access(node: Node, source: &str) -> Option<Visibility> {
    let text = node.utf8_text(source.as_bytes()).unwrap_or("");
    match text {
        "public:" | "public" => Some(Visibility::Public),
        "protected:" | "protected" => Some(Visibility::Protected),
        "private:" | "private" => Some(Visibility::Private),
        _ => None,
    }
}
