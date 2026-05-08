# Code Review Summary

Generated on 2026-05-07 from three read-only subagent reviews:

- `bugs.md`: correctness and behavioral defects
- `performance.md`: scalability, allocation, and IO hot paths
- `architecture.md`: data model, public API, and module boundaries

## Highest Priority Findings

### 1. Call graph resolution is structurally wrong

**Files:** `src/cpp_parser.rs`, `src/index.rs`, `src/db_impl.rs`

Calls are not associated with the current function because the parser never tracks function scope. The call graph stores caller names from namespace/class scope instead, and DB resolution later tries to match those strings to `definitions.qualified_name`. This makes `find_callers`, `find_callees`, and persisted `caller_id`/`callee_id` unreliable.

**Recommended direction:** Track current function/method qualified name during parsing and move toward stable symbol IDs for call edges.

### 2. C++ method definitions lose their class qualifiers

**Files:** `src/cpp_parser.rs`

Out-of-class definitions like `void Derived::run()` are indexed as `demo::run` free functions rather than `demo::Derived::run` methods. This breaks method lookup, call graph resolution, and class/member queries.

**Recommended direction:** Preserve qualified declarators and split them into parent scope plus symbol name when registering function definitions.

### 3. Persistence has incorrect ID semantics

**Files:** `src/types.rs`, `src/index.rs`, `src/db_impl.rs`

`Reference.def_id` means an in-memory vector index before persistence but a SQLite row ID after loading. Saving a loaded index can remap references incorrectly. `parent_id` is declared as an integer foreign key but receives a string scope name. Repeated saves also append duplicate rows and use `last_insert_rowid()` incorrectly after file upserts.

**Recommended direction:** Introduce stable typed IDs in memory, map them explicitly to DB row IDs inside `db_impl`, and define save semantics as replace-vs-append.

### 4. Parser misses or mislabels common C++ constructs

**Files:** `src/cpp_parser.rs`

Plain uninitialized variables/fields are often skipped, inheritance access is taken from current class visibility instead of the base clause, virtual inheritance is never detected, definition names can be recorded as references, and call references are duplicated.

**Recommended direction:** Add targeted fixture tests for these cases before refactoring parser extraction.

### 5. Hot paths allocate and query more than needed

**Files:** `src/cpp_parser.rs`, `src/index.rs`, `src/db_impl.rs`

Large function bodies are copied into `signature`, every `Location` clones the full path, `line_context` rebuilds all line slices per reference, DB loading performs N+1 file path queries, and search/reference-resolution clone strings repeatedly.

**Recommended direction:** Store signature headers only, preserve file metadata as first-class indexed files, precompute line spans, and preload/join file rows during DB load.

## Validation

`cargo test` was run after the review pass and passed:

- 3 integration tests passed
- 0 unit/doc tests present

The existing tests are smoke tests and do not currently cover the highest-risk findings above.
