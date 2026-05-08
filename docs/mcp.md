# xref-indexer MCP Server

This document is optimized for LLM agents that need to use `xref-indexer` through MCP.

## Summary

- Server binary: `xref-mcp`
- Transport: stdio JSON-RPC MCP
- Primary database: SQLite
- Default database path: `./.git/code-indexer/xrefs.sqlite3`
- Source language default: `cpp`
- Reference storage default: `none`
- Query tools auto-reindex by default
- Results are Tree-sitter structural candidates, not compiler-perfect semantic facts

## Build

```sh
cargo build --release --bin xref-mcp
```

Binary path after build:

```text
target/release/xref-mcp
```

## MCP Client Config

Use an absolute command path. Set the working directory to the repository root when the client supports `cwd`; this makes the default DB path resolve to the repo-local `.git/code-indexer/xrefs.sqlite3`.

```json
{
  "mcpServers": {
    "xref-indexer": {
      "command": "/absolute/path/to/xref-indexer/target/release/xref-mcp",
      "cwd": "/absolute/path/to/project"
    }
  }
}
```

If the client does not support `cwd`, pass `db` explicitly in tool calls.

## Database Rules

- Default DB path is relative to the MCP server process working directory.
- `xref_index`, `xref_reindex`, and saved CLI indexing persist index roots into the DB.
- Query tools use saved DB roots for auto-reindex.
- Query tools accept `roots` to override saved roots for that call.
- Query tools accept `no_reindex: true` to skip filesystem checks.
- If a legacy DB has file rows but no saved roots, queries infer roots from indexed file paths and repair the DB after reindex.

## Recommended Agent Workflow

1. Ensure an index exists.
2. Use `xref_find` for exact symbol definition lookup.
3. Use `xref_context` for source snippets around a definition or file/line.
4. Use `xref_callers`, `xref_callees`, or `xref_expand` to walk call graph edges.
5. Use `xref_hierarchy` for class base/derived relationships.
6. Use `xref_sql` only when a custom join is more direct than the high-level tools.
7. Prefer `no_reindex: true` only inside tight repeated-query loops when another recent query already reindexed.

## Common Arguments

These arguments are accepted by most query tools.

```json
{
  "db": "optional SQLite DB path",
  "roots": ["optional source roots for this auto-reindex check"],
  "limit": 20,
  "context_lines": 3,
  "snippets": true,
  "no_reindex": false
}
```

Notes:

- `db` defaults to `./.git/code-indexer/xrefs.sqlite3`.
- `roots` overrides saved DB roots for the current query.
- `context_lines` is the snippet radius around a hit.
- `snippets: false` suppresses snippets for compact responses.
- `no_reindex: true` avoids checking source files before the query.

## Index Tools

### xref_index

Build or rebuild a full DB.

Required:

```json
{
  "roots": ["path/to/src"]
}
```

Optional:

```json
{
  "db": ".git/code-indexer/xrefs.sqlite3",
  "language": "cpp",
  "references": "none",
  "threads": 8,
  "follow_symlinks": false,
  "reference_context": false
}
```

Reference modes:

- `none`: store definitions, call graph, and inheritance. `refs` table stays empty.
- `calls`: also store call-site references in `refs`.
- `all`: store broad identifier-like refs. Heavier and larger.

Example:

```json
{
  "roots": ["src", "include"],
  "db": ".git/code-indexer/xrefs.sqlite3",
  "language": "cpp",
  "references": "none"
}
```

### xref_reindex

Incrementally update an existing DB by parsing changed/new files and deleting removed files.

Required:

```json
{
  "roots": ["path/to/src"]
}
```

Optional arguments match `xref_index`.

Example:

```json
{
  "roots": ["src", "include"],
  "db": ".git/code-indexer/xrefs.sqlite3"
}
```

## Lookup Tools

### xref_find

Find definition candidates for a symbol.

Required:

```json
{
  "symbol": "demo::Derived::run"
}
```

Optional:

```json
{
  "include_search": false,
  "limit": 20,
  "context_lines": 3,
  "snippets": true,
  "db": ".git/code-indexer/xrefs.sqlite3",
  "no_reindex": false
}
```

Result interpretation:

- Prefer `confidence: exact_qualified`.
- Then prefer `confidence: exact_simple`.
- Treat `confidence: substring` as a fallback candidate.

Example:

```json
{
  "symbol": "WidgetFactory::Create",
  "limit": 5,
  "snippets": false
}
```

### xref_search

Case-insensitive substring search over definitions.

Required:

```json
{
  "pattern": "WidgetFactory"
}
```

Use when the exact symbol name is unknown.

### xref_refs

Query stored reference rows for a symbol.

Required:

```json
{
  "symbol": "compute"
}
```

Important:

- Default indexes use `references: none`, so `xref_refs` returns no rows.
- Rebuild with `references: calls` or `references: all` when this tool is needed.
- For call relationships, prefer `xref_callers`, `xref_callees`, or `xref_expand`.

### xref_callers

Find functions or methods that call a symbol.

Required:

```json
{
  "symbol": "compute"
}
```

Result source:

- Uses `call_graph`, not `refs`.
- Works with the default `references: none` mode.

### xref_callees

Find functions or methods called by a symbol.

Required:

```json
{
  "symbol": "demo::Derived::run"
}
```

Result source:

- Uses `call_graph`, not `refs`.
- Works with the default `references: none` mode.

### xref_expand

Expand a call graph around a seed symbol.

Required:

```json
{
  "symbol": "compute"
}
```

Optional:

```json
{
  "direction": "both",
  "depth": 2,
  "limit": 50,
  "snippets": false
}
```

Directions:

- `callers`: walk callers of the current frontier.
- `callees`: walk callees from the current frontier.
- `both`: walk both directions.

### xref_hierarchy

Find base and derived classes.

Required:

```json
{
  "class": "Derived"
}
```

Result fields:

- `bases`: direct base class candidates.
- `derived`: direct derived class candidates.

### xref_context

Fetch source context by symbol or by file/line.

By symbol:

```json
{
  "symbol": "demo::Derived::run",
  "context_lines": 5
}
```

By file/line:

```json
{
  "file": "tests/fixtures/demo.cpp",
  "line": 15,
  "context_lines": 5
}
```

Use file/line mode when a prior tool result already returned a precise location.

### xref_stats

Return DB summary, indexed files, and reindex status.

Example:

```json
{
  "db": ".git/code-indexer/xrefs.sqlite3",
  "no_reindex": true
}
```

### xref_sql

Run read-only SQL.

Required:

```json
{
  "sql": "SELECT name, qualified_name, kind FROM definitions LIMIT 10"
}
```

Allowed SQL starts:

- `SELECT`
- `WITH`
- `PRAGMA`

Example call graph query:

```json
{
  "sql": "SELECT caller_name, callee_name, f.path, cg.line FROM call_graph cg LEFT JOIN files f ON f.id = cg.file_id WHERE callee_name = 'compute' LIMIT 20"
}
```

## Response Shape

MCP tool responses contain both text and structured JSON.

```json
{
  "content": [
    {
      "type": "text",
      "text": "pretty-printed JSON result"
    }
  ],
  "structuredContent": {
    "ok": true,
    "command": "find"
  },
  "isError": false
}
```

On tool errors:

```json
{
  "content": [
    {
      "type": "text",
      "text": "error message"
    }
  ],
  "isError": true
}
```

## Reindex Response

Most query tools include a `reindex` object.

Checked and updated:

```json
{
  "checked": true,
  "source": "saved_config",
  "metrics": {
    "files_discovered": 10010,
    "files_indexed": 0,
    "total_ms": 209
  }
}
```

Skipped:

```json
{
  "checked": false,
  "reason": "disabled"
}
```

No saved roots:

```json
{
  "checked": false,
  "reason": "no_saved_roots"
}
```

`no_saved_roots` means the DB has no persisted roots and root inference failed. Fix by calling `xref_index` or `xref_reindex` with explicit `roots`.

## JSON-RPC Examples

Initialize:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"example","version":"0"}}}
```

List tools:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

Call `xref_find`:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"xref_find","arguments":{"symbol":"demo::Derived::run","limit":5,"snippets":false}}}
```

Call `xref_context` by file/line:

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"xref_context","arguments":{"file":"tests/fixtures/demo.cpp","line":15,"context_lines":5}}}
```

## Agent Guidance

- Use `xref_find` before reading source unless you already have file/line.
- Use `xref_callers` and `xref_callees` before broad text search.
- Use `xref_expand` for bounded graph walking around a ticket-relevant symbol.
- Use `xref_sql` for exact joins across `definitions`, `files`, `call_graph`, `inheritance`, and `refs`.
- Avoid `xref_refs` unless the DB was built with `references: calls` or `references: all`.
- Keep `snippets: false` for broad graph queries; fetch snippets later with `xref_context`.
- Leave auto-reindex enabled unless latency matters more than freshness for a tight query loop.

## Known Limits

- The index is Tree-sitter based.
- It does not require compiler commands or libclang.
- It may include structural false positives.
- It may miss semantic facts that require preprocessing, templates, overload resolution, or build configuration.
- It is intended to guide fast LLM code navigation, not replace compiler-grade C++ analysis.
