use rustc_hash::FxHashMap;

use crate::types::*;

/// In-memory cross-reference index.
pub struct Index {
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    calls: Vec<CallEdge>,
    inherits: Vec<InheritEdge>,

    defs_by_name: FxHashMap<String, Vec<usize>>,
    defs_by_qualified_name: FxHashMap<String, usize>,
    refs_by_name: FxHashMap<String, Vec<usize>>,
    callers: FxHashMap<String, Vec<usize>>,
    callees: FxHashMap<String, Vec<usize>>,
    inherits_from: FxHashMap<String, Vec<usize>>,
}

impl Index {
    pub fn new(
        definitions: Vec<Definition>,
        references: Vec<Reference>,
        calls: Vec<CallEdge>,
        inherits: Vec<InheritEdge>,
    ) -> Self {
        let mut index = Self {
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
        };
        index.build_lookups();
        index
    }

    fn build_lookups(&mut self) {
        for (i, def) in self.definitions.iter().enumerate() {
            self.defs_by_name
                .entry(def.name.clone())
                .or_default()
                .push(i);
            self.defs_by_qualified_name
                .insert(def.qualified_name.clone(), i);
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
        }
    }

    // ── Query methods ──

    pub fn find_definition(&self, name: &str) -> Vec<&Definition> {
        // Try qualified name first, then simple name.
        if let Some(&idx) = self.defs_by_qualified_name.get(name) {
            return vec![&self.definitions[idx]];
        }
        self.defs_by_name
            .get(name)
            .map(|indices| indices.iter().map(|&i| &self.definitions[i]).collect())
            .unwrap_or_default()
    }

    pub fn find_references(&self, name: &str) -> Vec<&Reference> {
        self.refs_by_name
            .get(name)
            .map(|indices| indices.iter().map(|&i| &self.references[i]).collect())
            .unwrap_or_default()
    }

    pub fn find_callers(&self, name: &str) -> Vec<&Definition> {
        let calls = self.callers.get(name);
        let mut result = Vec::new();
        if let Some(call_indices) = calls {
            for &ci in call_indices {
                let caller = &self.calls[ci].caller_name;
                if let Some(defs) = self.defs_by_name.get(caller.as_str()) {
                    for &di in defs {
                        result.push(&self.definitions[di]);
                    }
                }
            }
        }
        result
    }

    pub fn find_callees(&self, name: &str) -> Vec<&Definition> {
        let calls = self.callees.get(name);
        let mut result = Vec::new();
        if let Some(call_indices) = calls {
            for &ci in call_indices {
                let callee = &self.calls[ci].callee_name;
                if let Some(defs) = self.defs_by_name.get(callee.as_str()) {
                    for &di in defs {
                        result.push(&self.definitions[di]);
                    }
                }
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
        // Find the class definition, then look for inherits where it's the derived.
        for (i, inh) in self.inherits.iter().enumerate() {
            if self.inherits[i].derived_name.contains(class_name) {
                if let Some(defs) = self.defs_by_name.get(&inh.base_name) {
                    for &di in defs {
                        result.push(&self.definitions[di]);
                    }
                }
            }
        }
        result
    }

    fn direct_derived(&self, class_name: &str) -> Vec<&Definition> {
        let mut result = Vec::new();
        for inh in &self.inherits {
            if inh.base_name == class_name || inh.base_name.ends_with(&format!("::{class_name}")) {
                if let Some(defs) = self.defs_by_name.get(&inh.derived_name) {
                    for &di in defs {
                        // Only include if it has the derived_name as its name.
                        if self.definitions[di].name == inh.derived_name
                            || self.definitions[di].qualified_name == inh.derived_name
                        {
                            result.push(&self.definitions[di]);
                        }
                    }
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

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    pub fn reference_count(&self) -> usize {
        self.references.len()
    }
}

pub struct ClassHierarchy<'a> {
    pub bases: Vec<&'a Definition>,
    pub derived: Vec<&'a Definition>,
}

/// Builds an [Index] from [FileSymbols] produced by parsers.
pub struct IndexBuilder {
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    calls: Vec<CallEdge>,
    inherits: Vec<InheritEdge>,
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self {
            definitions: Vec::new(),
            references: Vec::new(),
            calls: Vec::new(),
            inherits: Vec::new(),
        }
    }

    pub fn add_file(&mut self, symbols: FileSymbols) {
        self.definitions.extend(symbols.definitions);
        self.references.extend(symbols.references);
        self.calls.extend(symbols.calls);
        self.inherits.extend(symbols.inherits);
    }

    pub fn build(mut self) -> Index {
        // Resolve references against definitions.
        self.resolve_references();
        Index::new(self.definitions, self.references, self.calls, self.inherits)
    }

    fn resolve_references(&mut self) {
        // Build a quick lookup: name → set of qualified names.
        let mut name_to_qualified: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut qualified_set: FxHashMap<String, usize> = FxHashMap::default();
        for (i, def) in self.definitions.iter().enumerate() {
            name_to_qualified
                .entry(def.name.clone())
                .or_default()
                .push(def.qualified_name.clone());
            qualified_set.insert(def.qualified_name.clone(), i);
        }

        for r in &mut self.references {
            if let Some(qnames) = name_to_qualified.get(&r.name) {
                if qnames.len() == 1 {
                    // Unambiguous: use the only match.
                    if let Some(&def_idx) = qualified_set.get(&qnames[0]) {
                        r.def_id = Some(def_idx as i64);
                    }
                }
                // If ambiguous (multiple definitions with same name), we leave def_id as None.
                // Future: use file/scope proximity to disambiguate.
            }
        }
    }
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}
