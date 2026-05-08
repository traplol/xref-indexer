# Performance Review

Read-only review focus: performance, scalability, memory behavior, allocation/cloning, IO/query hot paths, and avoidable repeated work.

## Findings

### 1. High - `src/cpp_parser.rs:226`

**Problem:** Function definitions store `self.node_text(node).to_string()` as `signature`, but `node` is the whole `function_definition`, so large function bodies are copied into memory and later persisted to SQLite as signatures. This can dominate index size and save/load IO on real codebases.

**Recommended fix:** Store only the declaration header/declarator text, or extract text up to the function body start. Keep full bodies out of `Definition::signature`.

**Confidence:** High

### 2. High - `src/cpp_parser.rs:130`

**Problem:** Every `Location` clones the same file path with `self.file.to_path_buf()`. Definitions, references, and call edges each carry their own `PathBuf`, so files with many identifiers repeatedly allocate and store identical paths.

**Recommended fix:** Intern file paths in the index, store a file id, or use shared storage such as `Arc<PathBuf>` internally. Convert to owned paths only at API boundaries if needed.

**Confidence:** High

### 3. High - `src/cpp_parser.rs:144`, `src/cpp_parser.rs:587`, `src/cpp_parser.rs:644`

**Problem:** `line_context` rebuilds `Vec<&str>` from `self.source.lines().collect()` for every reference and call reference. That makes context capture roughly `O(reference_count * file_line_count)` per file and allocates repeatedly on a hot parse path.

**Recommended fix:** Precompute line spans or line slices once in `ParseState::new`, then fetch context by index.

**Confidence:** High

### 4. High - `src/db_impl.rs:231`

**Problem:** `open_from_db` performs an N+1 query pattern for file paths: definitions, refs, and calls each query rows, then run `SELECT path FROM files WHERE id = ?1` per row at `src/db_impl.rs:242`, `src/db_impl.rs:280`, and `src/db_impl.rs:313`. Large databases will spend unnecessary time in repeated SQLite lookups.

**Recommended fix:** Join `files` in the main SELECTs, or preload `files` into a `HashMap<i64, PathBuf>` once and reuse it while materializing rows.

**Confidence:** High

### 5. Medium - `src/cpp_parser.rs:379`, `src/cpp_parser.rs:639`

**Problem:** A call callee is inserted as a reference in `handle_call`, then `walk_children` continues into the callee identifier and `handle_reference` records it again. This inflates reference counts, memory, reference resolution work, and DB writes by roughly one extra reference per call.

**Recommended fix:** Pick one path for call references. Either remove the reference insertion from `handle_call`, or mark the callee location as consumed so `handle_reference` skips it.

**Confidence:** High

### 6. Medium - `src/index.rs:142`, `src/index.rs:157`

**Problem:** `class_hierarchy` scans every inheritance edge for every query, while `inherits_from` is built at `src/index.rs:69` but not used. `direct_derived` also allocates `format!("::{class_name}")` inside the loop.

**Recommended fix:** Use `inherits_from` for direct bases, add a reverse base-to-derived lookup for derived queries, and compute any suffix string once per query.

**Confidence:** High

### 7. Medium - `src/index.rs:237`

**Problem:** Reference resolution clones every definition name and qualified name into `name_to_qualified`, then clones qualified names again into `qualified_set`. This doubles string storage during build even though indices are enough to resolve unambiguous names.

**Recommended fix:** Build `FxHashMap<&str or String, Vec<usize>>`, or track `name -> Option<usize>` where `None` means ambiguous, then assign `def_id` directly from definition indices.

**Confidence:** High

### 8. Medium - `src/db_impl.rs:213`

**Problem:** `save_to_db` inserts call and inheritance rows, then runs full-table correlated `UPDATE` statements to resolve ids through `definitions.qualified_name`. On large call graphs this repeats indexed lookups for every row after insertion.

**Recommended fix:** Build a qualified-name-to-db-id map while inserting definitions and populate `caller_id`, `callee_id`, `derived_id`, and `base_id` during the initial inserts.

**Confidence:** Medium

### 9. Medium - `src/index.rs:128`

**Problem:** `search_symbols` lowercases every definition name on every query, allocating once per definition per search. This is avoidable repeated work for interactive or repeated symbol search.

**Recommended fix:** Store a lowercase search key when building the index, or add a dedicated case-insensitive search index.

**Confidence:** High
