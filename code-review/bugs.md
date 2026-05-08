# Bugs Review

Read-only review focus: correctness bugs, crashes, bad results, test gaps that hide real bugs, and behavioral edge cases.

## Findings

### 1. High - `src/cpp_parser.rs:209` / `src/cpp_parser.rs:598`

**Problem:** Call graph caller names are usually wrong because function definitions never establish a current function scope. `handle_call` reads `scope_stack.last()`, which contains only namespace/class scopes. Calls inside `demo::compute` are recorded with caller `"demo"`, and calls in global `main` are recorded with an empty caller. As a result, `find_callers`, `find_callees`, and DB `caller_id` resolution return bad or missing results.

**Recommended fix:** Track current function/method independently while walking a `function_definition`, using the extracted function qualified name. Use that current function for `CallEdge.caller_name`.

**Confidence:** High

### 2. High - `src/cpp_parser.rs:209` / `src/cpp_parser.rs:724`

**Problem:** Out-of-class C++ method definitions are indexed as free functions. For `void Derived::run()`, `extract_name_from_declarator` drops the qualifier and returns only `run`; `qualified_name` then prefixes only the current namespace, producing `demo::run` instead of `demo::Derived::run`, and `is_method` stays false because no class scope is active.

**Recommended fix:** Preserve qualified declarators when extracting function definitions. Split `Derived::run` into parent scope and symbol name, classify it as a method/constructor/destructor, and emit the full qualified name.

**Confidence:** High

### 3. High - `src/db_impl.rs:87` / `src/db_impl.rs:100`

**Problem:** Saving to an existing database appends stale duplicate definitions, refs, calls, and inheritance rows instead of replacing the previous index. File ID mapping is also wrong on `ON CONFLICT DO UPDATE`: `last_insert_rowid()` is not a reliable way to get the existing row's ID after an update, so repeated saves can attach new rows to file ID `0` or a previous insert.

**Recommended fix:** Define save semantics explicitly. For "save this index to this DB", clear dependent tables in a transaction before inserting, or use a fresh DB. For file IDs, use `RETURNING id` or query `SELECT id FROM files WHERE path = ?` after the upsert.

**Confidence:** High

### 4. Medium - `src/cpp_parser.rs:421`

**Problem:** Simple uninitialized variables and fields are skipped. `handle_declaration` only records `init_declarator` and direct `pointer_declarator` nodes, so declarations like `int global_counter;`, `int value;`, `int extra_data;`, arrays, and many plain declarators are not indexed unless another initialized definition exists elsewhere.

**Recommended fix:** Handle the declaration's `declarator` field generically and extract identifiers from direct `identifier`, `field_identifier`, `array_declarator`, `pointer_declarator`, and reference declarators. Add tests for uninitialized globals and fields.

**Confidence:** High

### 5. Medium - `src/cpp_parser.rs:657`

**Problem:** Inheritance access and virtual inheritance are recorded incorrectly. `handle_inheritance` uses `current_visibility()` from the class body instead of parsing the base-class clause, so `class Derived : public Base` is stored as private for a class. `is_virtual` is always false.

**Recommended fix:** Parse each base specifier for `public`, `protected`, `private`, and `virtual`, falling back to class/struct default only when no access is specified.

**Confidence:** High

### 6. Medium - `src/index.rs:142`

**Problem:** `class_hierarchy` can return false bases because `direct_bases` matches derived classes with `contains(class_name)`. Asking for `Base` can match unrelated derived names like `DatabaseClient` or `BaseImpl`, returning inheritance edges for the wrong class.

**Recommended fix:** Use exact simple-name or qualified-name matching, consistent with `direct_derived`, instead of substring matching.

**Confidence:** High

### 7. Medium - `src/db_impl.rs:120`

**Problem:** `parent_id` is declared as an integer foreign key but the code inserts `d.parent.as_ref()`, which is a scope name string. SQLite accepts it because foreign keys are disabled during save, but SQL joins through `parent_id` are broken.

**Recommended fix:** Resolve parent qualified names to definition row IDs before inserting, or rename/change the column to `parent_name TEXT` if string storage is intended.

**Confidence:** High

### 8. Low - `src/cpp_parser.rs:229`

**Problem:** Function definition names are not marked as definition locations. The code inserts the location of the whole `function_definition`, then later computes `name_loc` for the emitted definition. On the second pass, the identifier at `name_loc` is collected as a reference, creating false self-references for function definitions.

**Recommended fix:** Insert `name_loc.line` and `name_loc.column` into `def_locations`, not the start of the whole function node.

**Confidence:** High

### 9. Low - `src/cpp_parser.rs:379`

**Problem:** Call references are duplicated. `handle_call` explicitly pushes a reference for the callee, then `walk_children` descends into the callee identifier and `handle_reference` records it again.

**Recommended fix:** Either let normal identifier traversal record call references, or mark the callee location as already handled before recursing.

**Confidence:** High
