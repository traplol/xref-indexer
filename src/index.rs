use rustc_hash::FxHashMap;
use serde::Serialize;

use crate::types::*;

/// In-memory cross-reference index.
pub struct Index {
    files: Vec<IndexedFile>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    calls: Vec<CallEdge>,
    inherits: Vec<InheritEdge>,

    defs_by_name: FxHashMap<String, Vec<DefId>>,
    defs_by_qualified_name: FxHashMap<String, DefId>,
    refs_by_name: FxHashMap<String, Vec<usize>>,
    callers: FxHashMap<String, Vec<usize>>,
    callees: FxHashMap<String, Vec<usize>>,
    inherits_from: FxHashMap<String, Vec<usize>>,
    /// Reverse lookup: base class name → inheritance edges where it's the base.
    base_of: FxHashMap<String, Vec<usize>>,
}

impl Index {
    pub fn new(
        files: Vec<IndexedFile>,
        definitions: Vec<Definition>,
        references: Vec<Reference>,
        calls: Vec<CallEdge>,
        inherits: Vec<InheritEdge>,
    ) -> Self {
        let mut index = Self {
            files,
            definitions,
            references,
            calls,
            inherits,
            defs_by_name: FxHashMap::default(),
            defs_by_qualified_name: FxHashMap::default(),
            refs_by_name: FxHashMap::default(),
            callers: FxHashMap::default(),
            callees: FxHashMap::default(),
            inherits_from: FxHashMap::default(),
            base_of: FxHashMap::default(),
        };
        index.build_lookups();
        index
    }

    fn build_lookups(&mut self) {
        for (i, def) in self.definitions.iter().enumerate() {
            let id = DefId(i);
            self.defs_by_name
                .entry(def.name.clone())
                .or_default()
                .push(id);
            self.defs_by_qualified_name
                .insert(def.qualified_name.clone(), id);
        }

        for (i, r) in self.references.iter().enumerate() {
            self.refs_by_name.entry(r.name.clone()).or_default().push(i);
        }

        for (i, call) in self.calls.iter().enumerate() {
            self.callers
                .entry(call.callee_name.clone())
                .or_default()
                .push(i);
            self.callees
                .entry(call.caller_name.clone())
                .or_default()
                .push(i);
        }

        for (i, inh) in self.inherits.iter().enumerate() {
            self.inherits_from
                .entry(inh.derived_name.clone())
                .or_default()
                .push(i);
            if let Some(simple) = simple_name(&inh.derived_name) {
                if simple != inh.derived_name {
                    self.inherits_from
                        .entry(simple.to_string())
                        .or_default()
                        .push(i);
                }
            }
            self.base_of
                .entry(inh.base_name.clone())
                .or_default()
                .push(i);
            if let Some(simple) = simple_name(&inh.base_name) {
                if simple != inh.base_name {
                    self.base_of.entry(simple.to_string()).or_default().push(i);
                }
            }
        }
    }

    /// Resolve a `DefId` to a `&Definition`.
    pub fn resolve(&self, id: DefId) -> &Definition {
        &self.definitions[id.0]
    }

    // ── Query methods ──

    pub fn find_definition(&self, name: &str) -> Vec<&Definition> {
        self.definitions_for_name(name)
    }

    fn definitions_for_name(&self, name: &str) -> Vec<&Definition> {
        if let Some(&id) = self.defs_by_qualified_name.get(name) {
            return vec![self.resolve(id)];
        }
        self.defs_by_name
            .get(name)
            .map(|ids| ids.iter().map(|&id| self.resolve(id)).collect())
            .unwrap_or_default()
    }

    pub fn find_references(&self, name: &str) -> Vec<&Reference> {
        self.refs_by_name
            .get(name)
            .map(|indices| indices.iter().map(|&i| &self.references[i]).collect())
            .unwrap_or_default()
    }

    pub fn find_callers(&self, name: &str) -> Vec<&Definition> {
        let mut result = Vec::new();
        if let Some(call_indices) = self.callers.get(name) {
            for &ci in call_indices {
                let caller = &self.calls[ci].caller_name;
                result.extend(self.definitions_for_name(caller));
            }
        }
        result
    }

    pub fn find_callees(&self, name: &str) -> Vec<&Definition> {
        let mut result = Vec::new();
        if let Some(call_indices) = self.callees.get(name) {
            for &ci in call_indices {
                let callee = &self.calls[ci].callee_name;
                result.extend(self.definitions_for_name(callee));
            }
        }
        result
    }

    pub fn search_symbols(&self, pattern: &str) -> Vec<&Definition> {
        let lower = pattern.to_lowercase();
        self.definitions
            .iter()
            .filter(|d| d.name.to_lowercase().contains(&lower))
            .collect()
    }

    pub fn class_hierarchy(&self, class_name: &str) -> ClassHierarchy<'_> {
        let bases = self.direct_bases(class_name);
        let derived = self.direct_derived(class_name);
        ClassHierarchy { bases, derived }
    }

    fn direct_bases(&self, class_name: &str) -> Vec<&Definition> {
        let mut result = Vec::new();
        // Find inheritance edges where this class is the derived.
        for &inh_idx in self
            .inherits_from
            .get(class_name)
            .iter()
            .flat_map(|v| v.iter())
        {
            let base_name = &self.inherits[inh_idx].base_name;
            result.extend(self.definitions_for_name(base_name));
        }
        result
    }

    fn direct_derived(&self, class_name: &str) -> Vec<&Definition> {
        let mut result = Vec::new();
        let suffix = format!("::{class_name}");
        for &inh_idx in self.base_of.get(class_name).iter().flat_map(|v| v.iter()) {
            let inh = &self.inherits[inh_idx];
            // Also check suffix match for qualified names.
            if inh.base_name != class_name && !inh.base_name.ends_with(&suffix) {
                continue;
            }
            for def in self.definitions_for_name(&inh.derived_name) {
                if def.name == inh.derived_name || def.qualified_name == inh.derived_name {
                    result.push(def);
                }
            }
        }
        result
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    pub fn calls(&self) -> &[CallEdge] {
        &self.calls
    }

    pub fn inherits(&self) -> &[InheritEdge] {
        &self.inherits
    }

    pub fn files(&self) -> &[IndexedFile] {
        &self.files
    }

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    pub fn reference_count(&self) -> usize {
        self.references.len()
    }
}

#[derive(Debug, Serialize)]
pub struct ClassHierarchy<'a> {
    pub bases: Vec<&'a Definition>,
    pub derived: Vec<&'a Definition>,
}

/// Builds an [Index] from [FileSymbols] produced by parsers.
pub struct IndexBuilder {
    files: Vec<IndexedFile>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    calls: Vec<CallEdge>,
    inherits: Vec<InheritEdge>,
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            definitions: Vec::new(),
            references: Vec::new(),
            calls: Vec::new(),
            inherits: Vec::new(),
        }
    }

    pub fn with_capacity(
        files: usize,
        definitions: usize,
        references: usize,
        calls: usize,
        inherits: usize,
    ) -> Self {
        Self {
            files: Vec::with_capacity(files),
            definitions: Vec::with_capacity(definitions),
            references: Vec::with_capacity(references),
            calls: Vec::with_capacity(calls),
            inherits: Vec::with_capacity(inherits),
        }
    }

    pub fn add_file(&mut self, symbols: FileSymbols) {
        self.files.push(IndexedFile {
            path: symbols.file,
            language: symbols.language,
        });
        self.definitions.extend(symbols.definitions);
        self.references.extend(symbols.references);
        self.calls.extend(symbols.calls);
        self.inherits.extend(symbols.inherits);
    }

    pub fn build(mut self) -> Index {
        // Resolve references against definitions.
        self.resolve_references();
        Index::new(
            self.files,
            self.definitions,
            self.references,
            self.calls,
            self.inherits,
        )
    }

    fn resolve_references(&mut self) {
        // Build borrowed lookups so large runs do not clone hundreds of
        // thousands of definition names just to resolve reference IDs.
        let mut qualified_to_id: FxHashMap<&str, usize> = FxHashMap::default();
        let mut unique_name_to_id: FxHashMap<&str, Option<usize>> = FxHashMap::default();
        for (i, def) in self.definitions.iter().enumerate() {
            qualified_to_id.insert(def.qualified_name.as_str(), i);
            unique_name_to_id
                .entry(def.name.as_str())
                .and_modify(|existing| *existing = None)
                .or_insert(Some(i));
        }

        for r in &mut self.references {
            if let Some(&def_idx) = qualified_to_id.get(r.name.as_str()) {
                r.def_id = Some(DefId(def_idx));
            } else if let Some(Some(def_idx)) = unique_name_to_id.get(r.name.as_str()) {
                // If ambiguous (multiple definitions with same name), leave def_id as None.
                // Future: use file/scope proximity to disambiguate.
                r.def_id = Some(DefId(*def_idx));
            }
        }
    }
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn simple_name(name: &str) -> Option<&str> {
    name.rsplit("::").next()
}
