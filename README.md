# xref-indexer

A Rust library that indexes C/C++ codebases and stores cross-references in SQLite — built for LLM consumption.

It finds definitions, references, call graphs, and class hierarchies for functions, methods, macros, variables, classes, structs, enums, namespaces, and typedefs. The output is a queryable SQLite database that an LLM (or any tool) can explore with arbitrary SQL.

## Quick start

```rust
use xref_indexer::{Indexer, Language};

let index = Indexer::builder()
    .add_directory("/path/to/codebase")
    .language(Language::Cpp)
    .build()
    .index()
    .expect("indexing failed");

// Save to SQLite so an LLM can query it.
let conn = index.save_to_db("index.db")?;

// Or query programmatically.
let defs = index.find_definition("MyClass");
let refs = index.find_references("important_function");
let callers = index.find_callers("helper");
let hierarchy = index.class_hierarchy("Base");
```

## CLI for LLM agents

The default binary is an LLM-first CLI: every command returns JSON and most lookup commands can include source snippets, so an agent can avoid separate `grep`/`find`/`sed -n` calls.

By default the CLI reads and writes:

```text
./.git/code-indexer/xrefs.sqlite3
```

Use `--db PATH` to override that location.

```sh
# Build an index at ./.git/code-indexer/xrefs.sqlite3.
cargo run --release -- index --root tests/fixtures --language cpp

# Benchmark a full first run with phase timings.
cargo run --release -- bench --root test-data --db /tmp/xrefs-bench.sqlite3 --pretty

# Update only new/changed/deleted files in an existing DB.
cargo run --release -- reindex --root test-data --db /tmp/xrefs-bench.sqlite3 --pretty

# Find exact definition candidates, with context.
cargo run --release -- find demo::Derived::run --pretty

# Walk xrefs without globbing through unrelated files.
cargo run --release -- callers compute --pretty
cargo run --release -- callees demo::Derived::run --pretty
cargo run --release -- expand compute --direction callers --depth 2 --pretty

# Fetch source context directly.
cargo run --release -- context --file tests/fixtures/demo.cpp --line 15 --context 4 --pretty

# Query the SQLite DB when a custom join is easier.
cargo run --release -- sql --sql "SELECT name, qualified_name, kind FROM definitions LIMIT 10" --pretty
```

DB-backed query commands run an incremental reindex check before reading SQLite. They use the roots and index options saved by `index`, `reindex`, or saved `bench --db` runs, so a normal `find`/`refs`/`callers` request sees edits made since the last command. Pass `--no-reindex` to skip the filesystem check, or pass `--root PATH` on a query to override the saved roots for that invocation.

The CLI reports confidence/provenance-style fields such as `exact_qualified`, `exact_simple`, `substring`, `resolved_unique_name`, and `structural_call`. The index is Tree-sitter based, so treat results as fast structural candidates rather than compiler-perfect C++ semantic facts.

The default reference mode is `--references calls`, which stores definitions, call graph edges, and call-site refs. Use `--references all` for exhaustive identifier refs, or `--references none` when the call graph table is enough. Per-reference source-line context is disabled by default; use `context`/snippets for lazy source text, or pass `--reference-context` for the heavier legacy behavior.

## Performance benchmarking

The library exposes `Indexer::index_with_metrics()` and `xref_indexer::benchmark::run(...)` for timed runs. The CLI `bench` command reports discovery, parse, build, SQLite save, row counts, byte counts, lossy-decoded file counts, thread count, and final DB size as JSON. `bench` is no-save by default unless `--db` or `--save` is provided.

On the local 10,010-file downloaded C++ corpus in `test-data`, release-mode first-run timing with the default fast reference mode was:

```text
files=10010 definitions=629972 calls=847774 refs=827067
index=9792ms save=8394ms total=18187ms
```

The high-level CLI lookup commands query SQLite directly instead of hydrating the full index into memory per invocation. With `--no-reindex` on the same 302 MB large index DB, representative release-mode raw query timings were `find` 3 ms, `refs` 3 ms, `callers` 3 ms, `callees` 5 ms, `search` 7 ms, `hierarchy` 24 ms, `stats` 34 ms, and `expand --depth 2 --limit 20` 124 ms.

Incremental `reindex` stores both content checksums and symbol-output fingerprints per file. DB-backed query commands run the same incremental check automatically before answering. A clean auto-reindexed `find` against the large DB checked 10,010 indexed files, parsed 0, and answered with `reindex.metrics.total_ms=437`. After appending comments to 250 random C/C++ files with `scripts/modify-random.py`, the same query parsed 217 changed indexed files and updated the DB before lookup in `847ms` total (`discover=122ms checksum=306ms parse=330ms db=4ms`) because the parsed symbols were unchanged.

For the leanest first run, `--references none` keeps `call_graph` but skips the duplicate call-site `refs` rows; on the same corpus that mode measured `total=13442ms`.

## SQLite schema

The database is designed so an LLM can join tables and explore the codebase without writing Rust code.

```sql
-- Every source file indexed.
CREATE TABLE files (
    id INTEGER PRIMARY KEY,
    path TEXT UNIQUE NOT NULL,
    language TEXT NOT NULL,
    checksum TEXT,
    symbols_checksum TEXT,
    indexed_at INTEGER DEFAULT (unixepoch())
);

-- Every symbol definition (function, class, variable, macro, enum, ...).
CREATE TABLE definitions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,   -- e.g. "demo::Base::run"
    kind TEXT NOT NULL,             -- function, method, class, enum, macro, ...
    file_id INTEGER REFERENCES files(id),
    line INTEGER NOT NULL,
    column INTEGER NOT NULL,
    end_line INTEGER,
    end_column INTEGER,
    parent_name TEXT,
    signature TEXT,                 -- function/method declaration header
    visibility TEXT DEFAULT 'public',
    is_definition INTEGER DEFAULT 1,
    extra TEXT                      -- JSON for language-specific data
);

-- Symbol references. Default mode stores call-site refs; --references all stores
-- exhaustive identifier-like refs.
CREATE TABLE refs (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file_id INTEGER REFERENCES files(id),
    line INTEGER NOT NULL,
    column INTEGER NOT NULL,
    def_id INTEGER REFERENCES definitions(id),  -- resolved definition, NULL if ambiguous
    context TEXT                                 -- optional; only set with --reference-context
);

-- Who calls whom.
CREATE TABLE call_graph (
    id INTEGER PRIMARY KEY,
    caller_id INTEGER REFERENCES definitions(id),
    callee_id INTEGER REFERENCES definitions(id),
    file_id INTEGER REFERENCES files(id),
    line INTEGER NOT NULL,
    caller_name TEXT,
    callee_name TEXT
);

-- Class inheritance.
CREATE TABLE inheritance (
    id INTEGER PRIMARY KEY,
    derived_id INTEGER REFERENCES definitions(id),
    base_id INTEGER REFERENCES definitions(id),
    file_id INTEGER REFERENCES files(id),
    access TEXT DEFAULT 'public',
    is_virtual INTEGER DEFAULT 0,
    derived_name TEXT,
    base_name TEXT
);
```

### Example SQL queries for an LLM

```sql
-- Find all definitions of a symbol.
SELECT * FROM definitions WHERE name = 'MyClass';

-- Find everywhere a function is called.
SELECT cg.line, f.path, d.signature
FROM call_graph cg
JOIN definitions d ON d.id = cg.callee_id
JOIN files f ON f.id = cg.file_id
WHERE d.name = 'compute';

-- Show a class hierarchy.
SELECT d.qualified_name, i.access
FROM inheritance i
JOIN definitions d ON d.id = i.base_id
WHERE i.derived_name = 'Base';

-- Find the most-referenced symbols.
SELECT d.name, d.qualified_name, d.kind, COUNT(*) as ref_count
FROM refs r
JOIN definitions d ON d.id = r.def_id
GROUP BY d.id
ORDER BY ref_count DESC
LIMIT 20;

-- Show the call chain from a given function.
WITH RECURSIVE call_chain AS (
    SELECT callee_name, caller_name, 1 AS depth
    FROM call_graph WHERE caller_name = 'main'
    UNION ALL
    SELECT cg.callee_name, cg.caller_name, cc.depth + 1
    FROM call_graph cg
    JOIN call_chain cc ON cg.caller_name = cc.callee_name
    WHERE cc.depth < 10
)
SELECT * FROM call_chain;
```

## Library API

### Indexer builder

```rust
let indexer = Indexer::builder()
    .add_directory("src/")
    .add_directory("include/")
    .language(Language::Cpp)       // or Language::C
    .follow_symlinks(false)
    .build();
```

### Querying the index

| Method | Returns | Description |
|--------|---------|-------------|
| `find_definition(name)` | `Vec<&Definition>` | Exact match on qualified or simple name |
| `find_references(name)` | `Vec<&Reference>` | All usage sites for a symbol |
| `find_callers(name)` | `Vec<&Definition>` | Functions that call this one |
| `find_callees(name)` | `Vec<&Definition>` | Functions called by this one |
| `search_symbols(pattern)` | `Vec<&Definition>` | Case-insensitive substring search |
| `class_hierarchy(name)` | `ClassHierarchy` | Base and derived classes |
| `definitions()` | `&[Definition]` | All definitions |
| `references()` | `&[Reference]` | All references |
| `calls()` | `&[CallEdge]` | All call edges |
| `inherits()` | `&[InheritEdge]` | All inheritance edges |

### Persistence

```rust
// Save.
let conn = index.save_to_db("index.db")?;

// Load.
let (index, conn) = Index::open_from_db("index.db")?;
```

## Symbol kinds

| Kind | C | C++ | Notes |
|------|---|-----|-------|
| `function` | yes | yes | Free functions |
| `method` | — | yes | Class members (non-ctor/dtor) |
| `constructor` | — | yes | |
| `destructor` | — | yes | |
| `macro` | yes | yes | `#define` and function-like macros |
| `variable` | yes | yes | Global and local variables |
| `field` | yes | yes | Struct/class member variables |
| `class` | — | yes | |
| `struct` | yes | yes | |
| `union` | yes | yes | |
| `enum` | yes | yes | |
| `enum_variant` | yes | yes | Individual enumerators |
| `typedef` | yes | yes | |
| `namespace` | — | yes | |

## What is resolved

- **Qualified names** — `namespace::ClassName::methodName` for every definition
- **Scope** — namespaces, classes, structs, and enums contribute to qualified names
- **Visibility** — `public`, `protected`, `private` tracked for class members
- **References** — unambiguous names (single definition) are resolved to `def_id`; ambiguous names are left unresolved
- **Call edges** — direct function calls (`f()`, `obj.method()`, `ns::func()`)
- **Inheritance** — `public`/`protected`/`private` base classes; template SFINAE patterns
- **Template declarations** — walked transparently; the inner class/function definition is found

## What is not (yet) resolved

- **Overload disambiguation** — multiple functions with the same name but different signatures are not distinguished
- **Using declarations** — `using ns::func` is not tracked
- **ADL / Koenig lookup** — call resolution through argument-dependent lookup
- **Cross-file call graph resolution** — call edges use short names; callers/callees are matched by qualified name in the DB post-pass
- **C `typedef` struct names** — `typedef struct { ... } Name` may not associate `Name` with the struct

## Architecture

```
                     ┌──────────────┐
                     │   Indexer    │  walkdir + parser selection
                     └──────┬───────┘
                            │
              ┌─────────────┼─────────────┐
              │             │             │
        ┌─────┴─────┐ ┌─────┴─────┐ ┌─────┴─────┐
        │   file1   │ │   file2   │ │   fileN   │
        └─────┬─────┘ └─────┬─────┘ └─────┬─────┘
              │             │             │
        ┌─────┴─────┐       │             │
        │ CppParser │       │             │
        │ (tree-    │       │             │
        │  sitter)  │       │             │
        └─────┬─────┘       │             │
              │             │             │
         FileSymbols    FileSymbols   FileSymbols
              │             │             │
              └─────────────┼─────────────┘
                            │
                    ┌───────┴───────┐
                    │ IndexBuilder  │  resolve refs → defs
                    └───────┬───────┘
                            │
                      ┌─────┴─────┐
                      │   Index   │  in-memory FxHashMap lookups
                      └─────┬─────┘
                            │
              ┌─────────────┼─────────────┐
              │             │             │
        ┌─────┴─────┐ ┌─────┴─────┐ ┌─────┴─────┐
        │ Query API │ │ save_to_db│ │open_from_db│
        └───────────┘ └─────┬─────┘ └─────┬─────┘
                            │             │
                      ┌─────┴─────┐ ┌─────┴─────┐
                      │  SQLite   │ │  SQLite   │
                      │  (write)  │ │  (read)   │
                      └───────────┘ └───────────┘
```

### Parsing strategy

Each file is parsed with [tree-sitter](https://tree-sitter.github.io/) in two passes:

1. **Definition pass** — walk the CST, track scope with a stack, collect definitions and mark their name locations
2. **Reference pass** — walk again, collect every identifier that is not a definition site as a reference; detect `call_expression` nodes for call edges

This avoids the complexity of distinguishing definition-site identifiers from reference-site identifiers in a single pass.

### Scope tracking

A scope stack is maintained during the tree walk. When a `namespace_definition`, `class_specifier`, `struct_specifier`, or `enum_specifier` is entered, its name is pushed. Qualified names are computed by joining the scope stack with `::`. Template declarations are walked transparently — the inner declaration sees the correct scope.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tree-sitter` | 0.26 | CST parsing engine |
| `tree-sitter-c` | 0.24 | C grammar |
| `tree-sitter-cpp` | 0.23 | C++ grammar |
| `rusqlite` | 0.39 (bundled) | SQLite storage |
| `walkdir` | 2.5 | Directory traversal |
| `rustc-hash` | 2.1 | FxHashMap (fast hashing) |
| `serde` / `serde_json` | 1 | JSON for `extra` columns |

## Running the examples

```bash
# Index fmtlib/fmt (clone first).
git clone --depth 1 https://github.com/fmtlib/fmt.git test-data/fmt
cargo run --example index_fmt

# Index nlohmann/json.
git clone --depth 1 https://github.com/nlohmann/json.git test-data/json
cargo run --example index_json
```

## Testing

```bash
cargo test
```

Integration tests index a small C++ fixture (namespace, class with constructor/destructor, methods, enum, global variable, macro, inheritance) and verify:

- All expected definitions are found with correct qualified names
- Inheritance edges exist
- Call graph edges exist
- References are collected
- SQLite round-trip preserves all data
