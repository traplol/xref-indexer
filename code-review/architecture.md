# Architecture Review

Read-only review focus: architecture, public API design, module boundaries, data model, maintainability, extensibility, and fit for the crate's purpose.

## Findings

### 1. High - `src/types.rs:114`, `src/index.rs:249`, `src/db_impl.rs:297`

**Problem:** `Reference.def_id` is public, but before persistence it stores an in-memory definition vector index. After `open_from_db`, the same field stores a SQLite row ID. `save_to_db` also assumes `def_id` is still a vector index, so saving a loaded index can remap references incorrectly.

**Recommended fix:** Introduce a stable `DefinitionId` newtype with one meaning in memory, and explicitly map to/from database row IDs only inside `db_impl`. Alternatively keep DB IDs out of public in-memory types.

**Confidence:** High

### 2. High - `src/types.rs:101`, `src/cpp_parser.rs:126`, `src/db_impl.rs:31`

**Problem:** `Definition.parent` is an `Option<String>`, but the SQLite schema calls the column `parent_id INTEGER REFERENCES definitions(id)`. The value inserted is a string parent name, not a resolved definition ID, and `parent_qualified_name()` currently returns only the last scope component. This makes symbol hierarchy unreliable and blocks future namespace/class tree queries.

**Recommended fix:** Decide whether the model stores `parent_name` or `parent_id`. Prefer resolving parents in `IndexBuilder` to a stable definition ID, then persist that ID with proper DB row remapping.

**Confidence:** High

### 3. High - `src/types.rs:119`, `src/cpp_parser.rs:601`, `src/db_impl.rs:216`

**Problem:** `CallEdge` and `InheritEdge` store names rather than symbol identities. `handle_call()` derives `caller_name` from the current scope stack, not from the current function definition, and DB resolution later matches names against `qualified_name`. This makes overloaded functions, methods defined out of class, same-name classes, and namespace-qualified calls hard to model correctly.

**Recommended fix:** Keep source spelling fields, but add resolved `caller_id`, `callee_id`, `derived_id`, and `base_id` in the in-memory model using stable IDs. Track current function/method scope during parsing or resolve edges in `IndexBuilder` from richer context.

**Confidence:** High

### 4. Medium - `src/types.rs:134`, `src/index.rs:224`, `src/db_impl.rs:103`

**Problem:** `FileSymbols` contains `file` and `language`, but `IndexBuilder::add_file()` drops them. Persistence reconstructs files only from definitions/references and hard-codes language as `"c"`, so symbol-free files disappear and C++ files are stored as C. This limits incremental indexing, coverage reporting, mixed-language support, and schema correctness.

**Recommended fix:** Add a first-class `IndexedFile` collection to `Index`, preserve `FileSymbols.file/language`, and derive DB `files` rows from that collection rather than from symbol locations.

**Confidence:** High

### 5. Medium - `src/types.rs:94`, `src/index.rs:21`

**Problem:** Core model structs expose all fields publicly, and `Index::new()` accepts arbitrary vectors. External callers can construct indexes with inconsistent IDs, unresolved edges, duplicate qualified-name collisions, or invalid `extra` JSON. This makes later internal model changes harder because storage details are already part of the public API.

**Recommended fix:** Narrow construction through a public builder or parser output API, use stable typed IDs, consider `#[non_exhaustive]` for public structs/enums, and expose validated query methods as the primary interface.

**Confidence:** Medium

### 6. Low - `src/lib.rs:43`, `src/lib.rs:117`

**Problem:** One `Language` applies to every path, extensions are fixed, and skipped directories are hard-coded inside `Indexer::index()`. That is workable for the current C/C++ prototype, but it will be difficult to index real mixed C/C++ trees, generated code, vendored code, or project-specific layouts without changing library code.

**Recommended fix:** Add include/exclude configuration, per-path or per-extension language overrides, and an optional file filter callback or pattern list. Keep default skips, but make them explicit builder policy.

**Confidence:** High
