use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row};
use serde::Serialize;
use serde_json::{json, Value};
use xref_indexer::benchmark::{self, BenchmarkConfig};
use xref_indexer::types::{CallEdge, Definition, Location, SymbolKind, Visibility};
use xref_indexer::{Indexer, Language, ReferenceMode};

const DEFAULT_DB_PATH: &str = ".git/code-indexer/xrefs.sqlite3";

#[derive(Debug, Default)]
struct ParsedArgs {
    flags: HashMap<String, Vec<String>>,
    positionals: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Snippet {
    file: String,
    focus_line: usize,
    start_line: usize,
    end_line: usize,
    text: String,
}

#[derive(Debug, Clone)]
struct DbReference {
    id: Option<i64>,
    name: String,
    kind: String,
    location: Location,
    def_id: Option<i64>,
    context: Option<String>,
}

#[derive(Debug)]
struct AutoReindexConfig {
    roots: Vec<PathBuf>,
    language: Language,
    follow_symlinks: bool,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
    source: &'static str,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return;
    }

    let command = &args[0];
    let parsed = match parse_args(&args[1..]) {
        Ok(parsed) => parsed,
        Err(err) => exit_json_error(&err),
    };

    let result = match command.as_str() {
        "index" => cmd_index(&parsed),
        "reindex" | "update" => cmd_reindex(&parsed),
        "bench" | "benchmark" => cmd_benchmark(&parsed),
        "stats" => cmd_stats(&parsed),
        "find" | "def" | "definitions" => cmd_find(&parsed),
        "search" => cmd_search(&parsed),
        "refs" | "references" => cmd_refs(&parsed),
        "callers" => cmd_callers(&parsed),
        "callees" => cmd_callees(&parsed),
        "hierarchy" => cmd_hierarchy(&parsed),
        "context" | "snippet" => cmd_context(&parsed),
        "expand" => cmd_expand(&parsed),
        "sql" | "query" => cmd_sql(&parsed),
        other => Err(format!("unknown command '{other}'")),
    };

    match result {
        Ok(value) => emit_json(value, parsed.bool_flag("pretty")),
        Err(err) => exit_json_error(&err),
    }
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut parsed = ParsedArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                return Err("empty option '--'".to_string());
            }
            if let Some((key, value)) = rest.split_once('=') {
                parsed
                    .flags
                    .entry(key.to_string())
                    .or_default()
                    .push(value.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                parsed
                    .flags
                    .entry(rest.to_string())
                    .or_default()
                    .push(args[i + 1].clone());
                i += 1;
            } else {
                parsed
                    .flags
                    .entry(rest.to_string())
                    .or_default()
                    .push("true".to_string());
            }
        } else if let Some(short) = arg.strip_prefix('-') {
            match short {
                "h" => parsed
                    .flags
                    .entry("help".to_string())
                    .or_default()
                    .push("true".to_string()),
                _ => return Err(format!("unsupported short option '-{short}'")),
            }
        } else {
            parsed.positionals.push(arg.clone());
        }
        i += 1;
    }
    Ok(parsed)
}

impl ParsedArgs {
    fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    fn values(&self, name: &str) -> impl Iterator<Item = &str> {
        self.flags
            .get(name)
            .into_iter()
            .flat_map(|values| values.iter().map(String::as_str))
    }

    fn bool_flag(&self, name: &str) -> bool {
        self.value(name)
            .map(|value| value != "false" && value != "0")
            .unwrap_or(false)
    }

    fn has_flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    fn required_value(&self, name: &str) -> Result<&str, String> {
        self.value(name)
            .ok_or_else(|| format!("missing required --{name}"))
    }

    fn db_path(&self) -> &str {
        self.value("db").unwrap_or(DEFAULT_DB_PATH)
    }

    fn first_symbol(&self) -> Result<&str, String> {
        self.positionals
            .first()
            .map(String::as_str)
            .or_else(|| self.value("name"))
            .or_else(|| self.value("symbol"))
            .ok_or_else(|| "missing symbol name".to_string())
    }

    fn limit(&self) -> usize {
        self.value("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(20)
    }

    fn radius(&self) -> usize {
        self.value("context")
            .or_else(|| self.value("radius"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(3)
    }

    fn threads(&self) -> Option<usize> {
        self.value("threads").and_then(|v| v.parse().ok())
    }

    fn reference_mode(&self) -> Result<ReferenceMode, String> {
        if self.bool_flag("all-references") {
            return Ok(ReferenceMode::All);
        }
        parse_reference_mode(
            self.value("references")
                .or_else(|| self.value("reference-mode"))
                .unwrap_or("none"),
        )
    }
}

fn roots_from_args(args: &ParsedArgs, command: &str) -> Result<Vec<PathBuf>, String> {
    let mut roots: Vec<PathBuf> = args.values("root").map(PathBuf::from).collect();
    roots.extend(args.values("path").map(PathBuf::from));
    roots.extend(args.positionals.iter().map(PathBuf::from));
    if roots.is_empty() {
        return Err(format!(
            "{command} requires at least one --root PATH or positional PATH"
        ));
    }
    Ok(roots)
}

fn cmd_index(args: &ParsedArgs) -> Result<Value, String> {
    if args.bool_flag("incremental") {
        return cmd_reindex(args);
    }

    let roots = roots_from_args(args, "index")?;

    let language = parse_language(args.value("language").or_else(|| args.value("lang")))?;
    let mut builder = Indexer::builder()
        .language(language)
        .follow_symlinks(args.bool_flag("follow-symlinks"))
        .reference_mode(args.reference_mode()?)
        .reference_context(args.bool_flag("reference-context"));
    if let Some(threads) = args.threads() {
        builder = builder.threads(threads);
    }
    for root in &roots {
        builder = builder.add_directory(root);
    }

    let db = args.db_path();
    ensure_db_parent(db)?;
    let run = builder
        .build()
        .index_db(db)
        .map_err(|e| format!("index failed: {e}"))?;

    Ok(json!({
        "ok": true,
        "command": "index",
        "db": db,
        "roots": roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "language": language_name(language),
        "summary": run.summary,
        "metrics": run.metrics,
        "save_ms": run.save_ms,
        "writer": run.writer,
        "total_ms": run.total_ms,
    }))
}

fn cmd_reindex(args: &ParsedArgs) -> Result<Value, String> {
    let roots = roots_from_args(args, "reindex")?;
    let language = parse_language(args.value("language").or_else(|| args.value("lang")))?;
    let db = args.db_path();
    ensure_db_parent(db)?;

    let mut builder = Indexer::builder()
        .language(language)
        .follow_symlinks(args.bool_flag("follow-symlinks"))
        .reference_mode(args.reference_mode()?)
        .reference_context(args.bool_flag("reference-context"));
    if let Some(threads) = args.threads() {
        builder = builder.threads(threads);
    }
    for root in &roots {
        builder = builder.add_directory(root);
    }

    let run = builder
        .build()
        .reindex_db(db)
        .map_err(|e| format!("reindex failed: {e}"))?;

    Ok(json!({
        "ok": true,
        "command": "reindex",
        "db": db,
        "roots": roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "language": language_name(language),
        "metrics": run.metrics,
    }))
}

fn cmd_benchmark(args: &ParsedArgs) -> Result<Value, String> {
    let roots = roots_from_args(args, "benchmark")?;
    let language = parse_language(args.value("language").or_else(|| args.value("lang")))?;
    let should_save =
        !args.bool_flag("no-save") && (args.bool_flag("save") || args.value("db").is_some());
    let db_path = should_save.then(|| PathBuf::from(args.db_path()));

    if let Some(db) = &db_path {
        ensure_db_parent(db.to_string_lossy().as_ref())?;
    }

    let report = benchmark::run(BenchmarkConfig {
        roots,
        language,
        follow_symlinks: args.bool_flag("follow-symlinks"),
        threads: args.threads(),
        include_reference_context: args.bool_flag("reference-context"),
        reference_mode: args.reference_mode()?,
        db_path,
    })?;

    Ok(json!({
        "ok": true,
        "command": "benchmark",
        "report": report,
    }))
}

fn maybe_auto_reindex(args: &ParsedArgs) -> Result<Value, String> {
    if args.bool_flag("no-reindex") || args.bool_flag("no-auto-reindex") {
        return Ok(json!({
            "checked": false,
            "reason": "disabled",
        }));
    }

    let db = args.db_path();
    let config = match auto_reindex_config(args, db)? {
        Some(config) => config,
        None => {
            return Ok(json!({
                "checked": false,
                "reason": "no_saved_roots",
            }));
        }
    };

    ensure_db_parent(db)?;
    let mut builder = Indexer::builder()
        .language(config.language)
        .follow_symlinks(config.follow_symlinks)
        .reference_mode(config.reference_mode)
        .reference_context(config.include_reference_context);
    if let Some(threads) = args.threads() {
        builder = builder.threads(threads);
    }
    for root in &config.roots {
        builder = builder.add_directory(root);
    }

    let run = builder
        .build()
        .reindex_db(db)
        .map_err(|e| format!("auto-reindex failed: {e}"))?;

    Ok(json!({
        "checked": true,
        "source": config.source,
        "db": db,
        "roots": config.roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "language": language_name(config.language),
        "metrics": run.metrics,
    }))
}

fn auto_reindex_config(args: &ParsedArgs, db: &str) -> Result<Option<AutoReindexConfig>, String> {
    let explicit_roots = query_roots_from_args(args);
    if !explicit_roots.is_empty() {
        return Ok(Some(AutoReindexConfig {
            roots: explicit_roots,
            language: parse_language(args.value("language").or_else(|| args.value("lang")))?,
            follow_symlinks: args.bool_flag("follow-symlinks"),
            reference_mode: args.reference_mode()?,
            include_reference_context: args.bool_flag("reference-context"),
            source: "query_args",
        }));
    }

    if !Path::new(db).exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("failed to open database '{db}' for auto-reindex: {e}"))?;
    let stored = xref_indexer::db::load_index_config(&conn)
        .map_err(|e| format!("failed to load saved index roots from '{db}': {e}"))?;
    drop(conn);

    let Some(stored) = stored else {
        return Ok(None);
    };

    let language = parse_language(
        args.value("language")
            .or_else(|| args.value("lang"))
            .or(Some(stored.language.as_str())),
    )?;
    let reference_mode = if has_reference_mode_override(args) {
        args.reference_mode()?
    } else {
        parse_reference_mode(&stored.reference_mode)?
    };
    let follow_symlinks = if args.has_flag("follow-symlinks") {
        args.bool_flag("follow-symlinks")
    } else {
        stored.follow_symlinks
    };
    let include_reference_context = if args.has_flag("reference-context") {
        args.bool_flag("reference-context")
    } else {
        stored.include_reference_context
    };

    Ok(Some(AutoReindexConfig {
        roots: stored.roots,
        language,
        follow_symlinks,
        reference_mode,
        include_reference_context,
        source: "saved_config",
    }))
}

fn query_roots_from_args(args: &ParsedArgs) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = args.values("root").map(PathBuf::from).collect();
    roots.extend(args.values("path").map(PathBuf::from));
    roots
}

fn has_reference_mode_override(args: &ParsedArgs) -> bool {
    args.has_flag("all-references")
        || args.has_flag("references")
        || args.has_flag("reference-mode")
}

fn cmd_stats(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    Ok(json!({
        "ok": true,
        "command": "stats",
        "db": db,
        "reindex": reindex,
        "summary": db_summary(&conn)?,
        "files": db_files(&conn)?,
    }))
}

fn cmd_find(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let limit = args.limit();
    let radius = args.radius();
    let include_snippets = !args.bool_flag("no-snippets");

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for def in db_definitions_for_name(&conn, query, Some(limit))? {
        if seen.insert(def.qualified_name.clone()) {
            results.push(def_hit(
                &def,
                query,
                results.len() + 1,
                include_snippets,
                radius,
            ));
        }
    }

    if results.is_empty() || args.bool_flag("include-search") {
        for def in db_search_definitions(&conn, query, limit.saturating_mul(4).max(limit))? {
            if results.len() >= limit {
                break;
            }
            if seen.insert(def.qualified_name.clone()) {
                results.push(def_hit(
                    &def,
                    query,
                    results.len() + 1,
                    include_snippets,
                    radius,
                ));
            }
        }
    }
    results.truncate(limit);

    Ok(json!({
        "ok": true,
        "command": "find",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": results.len(),
        "results": results,
        "notes": ["Tree-sitter structural candidates; prefer exact_qualified/exact_simple before substring results"]
    }))
}

fn cmd_search(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let include_snippets = !args.bool_flag("no-snippets");
    let radius = args.radius();
    let results: Vec<Value> = db_search_definitions(&conn, query, args.limit())?
        .iter()
        .take(args.limit())
        .enumerate()
        .map(|(i, def)| def_hit(def, query, i + 1, include_snippets, radius))
        .collect();

    Ok(json!({
        "ok": true,
        "command": "search",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": results.len(),
        "results": results,
        "notes": ["Default indexes do not populate refs; rebuild with --references calls or --references all for refs results"]
    }))
}

fn cmd_refs(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let include_snippets = !args.bool_flag("no-snippets");
    let radius = args.radius();
    let refs = db_refs_for_query(&conn, query, args.limit())?;
    let results: Vec<Value> = refs
        .iter()
        .enumerate()
        .map(|(i, r)| db_ref_hit(r, i + 1, include_snippets, radius))
        .collect();

    Ok(json!({
        "ok": true,
        "command": "refs",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": results.len(),
        "results": results,
    }))
}

fn cmd_callers(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let defs = db_call_endpoint_defs(&conn, query, true, args.limit())?;
    Ok(json!({
        "ok": true,
        "command": "callers",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": defs.len(),
        "results": owned_definition_hits_with_confidence(
            &defs,
            "structural_call_endpoint",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
    }))
}

fn cmd_callees(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let defs = db_call_endpoint_defs(&conn, query, false, args.limit())?;
    Ok(json!({
        "ok": true,
        "command": "callees",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": defs.len(),
        "results": owned_definition_hits_with_confidence(
            &defs,
            "structural_call_endpoint",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
    }))
}

fn cmd_hierarchy(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let (bases, derived) = db_class_hierarchy(&conn, query)?;
    Ok(json!({
        "ok": true,
        "command": "hierarchy",
        "db": db,
        "reindex": reindex,
        "query": query,
        "bases": owned_definition_hits_with_confidence(
            &bases,
            "inheritance_edge",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
        "derived": owned_definition_hits_with_confidence(
            &derived,
            "inheritance_edge",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
    }))
}

fn cmd_context(args: &ParsedArgs) -> Result<Value, String> {
    let radius = args.radius();
    if let Some(file) = args.value("file") {
        let line = args
            .required_value("line")?
            .parse::<usize>()
            .map_err(|_| "--line must be a positive integer".to_string())?;
        return Ok(json!({
            "ok": true,
            "command": "context",
            "target": { "file": file, "line": line },
            "snippet": read_snippet(Path::new(file), line, radius),
        }));
    }

    let db = args.db_path();
    let reindex = maybe_auto_reindex(args)?;
    let conn = open_query_conn(db)?;
    let query = args.first_symbol()?;
    let results: Vec<Value> = db_definitions_for_name(&conn, query, Some(args.limit()))?
        .iter()
        .map(|def| {
            json!({
                "definition": definition_json(def),
                "snippet": read_snippet(&def.location.file, def.location.line, radius),
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "command": "context",
        "db": db,
        "reindex": reindex,
        "query": query,
        "result_count": results.len(),
        "results": results,
    }))
}

fn cmd_expand(args: &ParsedArgs) -> Result<Value, String> {
    let reindex = maybe_auto_reindex(args)?;
    let db = args.db_path();
    let conn = open_query_conn(db)?;
    let seed = args.first_symbol()?;
    let depth = args
        .value("depth")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let direction = args.value("direction").unwrap_or("both");
    let limit = args.limit().max(1);
    let include_snippets = !args.bool_flag("no-snippets");
    let radius = args.radius();

    let mut frontier: HashSet<String> = db_aliases_for_seed(&conn, seed)?.into_iter().collect();
    let mut visited_symbols = frontier.clone();
    let mut seen_edges = HashSet::new();
    let mut edges = Vec::new();

    for level in 1..=depth {
        let mut next = HashSet::new();
        for call in db_call_edges_for_frontier(&conn, &frontier, direction)? {
            if edges.len() >= limit {
                break;
            }
            if matches!(direction, "callers" | "both") && matches_any(&call.callee_name, &frontier)
            {
                let key = edge_key(&call);
                if seen_edges.insert(key) {
                    edges.push(call_hit(&call, level, include_snippets, radius));
                    next.insert(call.caller_name.clone());
                }
            }
            if matches!(direction, "callees" | "both") && matches_any(&call.caller_name, &frontier)
            {
                let key = edge_key(&call);
                if seen_edges.insert(key) {
                    edges.push(call_hit(&call, level, include_snippets, radius));
                    next.insert(call.callee_name.clone());
                }
            }
        }
        next.retain(|name| visited_symbols.insert(name.clone()));
        if next.is_empty() || edges.len() >= limit {
            break;
        }
        frontier = next;
    }

    Ok(json!({
        "ok": true,
        "command": "expand",
        "db": db,
        "reindex": reindex,
        "seed": seed,
        "direction": direction,
        "depth": depth,
        "edge_count": edges.len(),
        "edges": edges,
        "notes": ["Call edges are Tree-sitter structural candidates; callee names may be simple names for member/qualified calls"]
    }))
}

fn cmd_sql(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let sql = args
        .value("sql")
        .or_else(|| args.value("query"))
        .map(str::to_string)
        .unwrap_or_else(|| args.positionals.join(" "));
    if sql.trim().is_empty() {
        return Err("sql requires --sql QUERY or a positional SQL string".to_string());
    }
    let first_word = sql
        .trim_start()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(first_word.as_str(), "select" | "with" | "pragma") {
        return Err(
            "sql command is read-only; query must start with SELECT, WITH, or PRAGMA".to_string(),
        );
    }

    let reindex = maybe_auto_reindex(args)?;
    let conn = open_query_conn(db)?;
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare SQL: {e}"))?;
    let col_count = stmt.column_count();
    let columns: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let limit = args.limit();
    let rows_iter = stmt
        .query_map([], |row| {
            let mut obj = serde_json::Map::new();
            for (i, name) in columns.iter().enumerate() {
                let value: SqlValue = row.get(i)?;
                obj.insert(name.clone(), sql_value_json(value));
            }
            Ok(Value::Object(obj))
        })
        .map_err(|e| format!("query failed: {e}"))?;
    let rows: Vec<Value> = rows_iter
        .take(limit)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to read query row: {e}"))?;

    Ok(json!({
        "ok": true,
        "command": "sql",
        "db": db,
        "reindex": reindex,
        "columns": columns,
        "row_count": rows.len(),
        "rows": rows,
    }))
}

fn open_query_conn(db: &str) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("failed to open database '{db}': {e}"))?;
    xref_indexer::db::create_schema(&conn)
        .map_err(|e| format!("failed to prepare database schema '{db}': {e}"))?;
    conn.execute_batch(
        "
        PRAGMA foreign_keys=ON;
        PRAGMA temp_store=MEMORY;
        PRAGMA cache_size=-200000;
        PRAGMA mmap_size=268435456;
        PRAGMA query_only=ON;
        ",
    )
    .map_err(|e| format!("failed to configure query database '{db}': {e}"))?;
    Ok(conn)
}

fn db_summary(conn: &Connection) -> Result<Value, String> {
    conn.query_row(
        "
        SELECT
            (SELECT COUNT(*) FROM files),
            (SELECT COUNT(*) FROM definitions),
            (SELECT COUNT(*) FROM refs),
            (SELECT COUNT(*) FROM call_graph),
            (SELECT COUNT(*) FROM inheritance)
        ",
        [],
        |row| {
            Ok(json!({
                "files": row.get::<_, i64>(0)?,
                "definitions": row.get::<_, i64>(1)?,
                "references": row.get::<_, i64>(2)?,
                "calls": row.get::<_, i64>(3)?,
                "inheritance_edges": row.get::<_, i64>(4)?,
            }))
        },
    )
    .map_err(|e| format!("failed to read database summary: {e}"))
}

fn db_files(conn: &Connection) -> Result<Vec<Value>, String> {
    let mut stmt = conn
        .prepare("SELECT path, language FROM files ORDER BY id")
        .map_err(|e| format!("failed to prepare files query: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(json!({
                "path": row.get::<_, String>(0)?,
                "language": row.get::<_, String>(1)?,
            }))
        })
        .map_err(|e| format!("failed to query files: {e}"))?;
    collect_sql_rows(rows, "files")
}

const DEFINITION_SELECT: &str = "
    SELECT
        d.id,
        d.name,
        d.qualified_name,
        d.kind,
        f.path,
        d.line,
        d.column,
        d.end_line,
        d.end_column,
        d.parent_name,
        d.signature,
        d.visibility,
        d.is_definition,
        d.extra
    FROM definitions d
    LEFT JOIN files f ON f.id = d.file_id
";

fn db_definition_by_id(conn: &Connection, id: i64) -> Result<Option<Definition>, String> {
    let sql = format!("{DEFINITION_SELECT} WHERE d.id = ?1 LIMIT 1");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare definition-by-id query: {e}"))?;
    stmt.query_row(params![id], row_to_definition)
        .optional()
        .map_err(|e| format!("failed to query definition id {id}: {e}"))
}

fn db_definition_by_qualified(
    conn: &Connection,
    qualified_name: &str,
) -> Result<Option<Definition>, String> {
    let sql = format!("{DEFINITION_SELECT} WHERE d.qualified_name = ?1 ORDER BY d.id LIMIT 1");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare qualified definition query: {e}"))?;
    stmt.query_row(params![qualified_name], row_to_definition)
        .optional()
        .map_err(|e| format!("failed to query definition '{qualified_name}': {e}"))
}

fn db_definitions_by_simple(
    conn: &Connection,
    name: &str,
    limit: Option<usize>,
) -> Result<Vec<Definition>, String> {
    if matches!(limit, Some(0)) {
        return Ok(Vec::new());
    }
    let sql = format!("{DEFINITION_SELECT} WHERE d.name = ?1 ORDER BY d.id LIMIT ?2");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare simple definition query: {e}"))?;
    let rows = stmt
        .query_map(params![name, sql_limit(limit)], row_to_definition)
        .map_err(|e| format!("failed to query definitions named '{name}': {e}"))?;
    collect_sql_rows(rows, "definitions")
}

fn db_definitions_for_name(
    conn: &Connection,
    name: &str,
    limit: Option<usize>,
) -> Result<Vec<Definition>, String> {
    if matches!(limit, Some(0)) {
        return Ok(Vec::new());
    }
    if let Some(def) = db_definition_by_qualified(conn, name)? {
        return Ok(vec![def]);
    }
    db_definitions_by_simple(conn, name, limit)
}

fn db_search_definitions(
    conn: &Connection,
    pattern: &str,
    limit: usize,
) -> Result<Vec<Definition>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "{DEFINITION_SELECT} WHERE lower(d.name) LIKE ?1 ESCAPE '\\' ORDER BY d.id LIMIT ?2"
    );
    let like_pattern = like_contains_pattern(pattern);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare symbol search query: {e}"))?;
    let rows = stmt
        .query_map(params![like_pattern, limit as i64], row_to_definition)
        .map_err(|e| format!("failed to search symbols for '{pattern}': {e}"))?;
    collect_sql_rows(rows, "symbol search")
}

fn row_to_definition(row: &Row<'_>) -> rusqlite::Result<Definition> {
    let kind = row.get::<_, String>(3)?;
    let visibility = row.get::<_, String>(11)?;
    Ok(Definition {
        id: row.get(0)?,
        name: row.get(1)?,
        qualified_name: row.get(2)?,
        kind: SymbolKind::from_str(&kind).unwrap_or(SymbolKind::Function),
        location: Location {
            file: row
                .get::<_, Option<String>>(4)?
                .map(PathBuf::from)
                .unwrap_or_default(),
            line: row.get::<_, i64>(5)? as usize,
            column: row.get::<_, i64>(6)? as usize,
            end_line: row.get::<_, i64>(7)? as usize,
            end_column: row.get::<_, i64>(8)? as usize,
        },
        parent: row.get(9)?,
        signature: row.get(10)?,
        visibility: Visibility::from_str(&visibility).unwrap_or(Visibility::Public),
        is_definition: row.get::<_, i64>(12)? != 0,
        extra: row.get(13)?,
    })
}

fn db_refs_for_query(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<DbReference>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut names = query_aliases(query);
    names.truncate(2);

    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let mut stmt = conn
        .prepare(
            "
            SELECT r.id, r.name, r.kind, f.path, r.line, r.column, r.def_id, r.context
            FROM refs r
            LEFT JOIN files f ON f.id = r.file_id
            WHERE r.name = ?1
            ORDER BY r.id
            ",
        )
        .map_err(|e| format!("failed to prepare references query: {e}"))?;

    for name in names {
        let rows = stmt
            .query_map(params![name], row_to_db_reference)
            .map_err(|e| format!("failed to query references: {e}"))?;
        for row in rows {
            let reference = row.map_err(|e| format!("failed to read reference row: {e}"))?;
            let key = (
                reference.location.file.clone(),
                reference.location.line,
                reference.location.column,
                reference.name.clone(),
            );
            if seen.insert(key) {
                refs.push(reference);
                if refs.len() >= limit {
                    return Ok(refs);
                }
            }
        }
    }
    Ok(refs)
}

fn row_to_db_reference(row: &Row<'_>) -> rusqlite::Result<DbReference> {
    Ok(DbReference {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        location: Location {
            file: row
                .get::<_, Option<String>>(3)?
                .map(PathBuf::from)
                .unwrap_or_default(),
            line: row.get::<_, i64>(4)? as usize,
            column: row.get::<_, i64>(5)? as usize,
            end_line: 0,
            end_column: 0,
        },
        def_id: row.get(6)?,
        context: row.get(7)?,
    })
}

fn db_call_endpoint_defs(
    conn: &Connection,
    query: &str,
    callers: bool,
    limit: usize,
) -> Result<Vec<Definition>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let endpoint_col = if callers {
        "callee_name"
    } else {
        "caller_name"
    };
    let target_id_col = if callers { "caller_id" } else { "callee_id" };
    let target_name_col = if callers {
        "caller_name"
    } else {
        "callee_name"
    };
    let sql = format!(
        "SELECT {target_id_col}, {target_name_col} FROM call_graph WHERE {endpoint_col} = ?1 ORDER BY id"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare call endpoint query: {e}"))?;
    let mut defs = Vec::new();
    let mut seen = HashSet::new();
    let mut def_cache_by_id: HashMap<i64, Option<Definition>> = HashMap::new();
    let mut def_cache_by_name: HashMap<String, Vec<Definition>> = HashMap::new();

    for name in query_aliases(query) {
        let rows = stmt
            .query_map(params![name], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })
            .map_err(|e| format!("failed to query call endpoints: {e}"))?;
        for row in rows {
            let (target_id, target_name) =
                row.map_err(|e| format!("failed to read call endpoint row: {e}"))?;
            let mut candidates = Vec::new();
            if let Some(id) = target_id {
                let cached = match def_cache_by_id.get(&id) {
                    Some(def) => def.clone(),
                    None => {
                        let def = db_definition_by_id(conn, id)?;
                        def_cache_by_id.insert(id, def.clone());
                        def
                    }
                };
                if let Some(def) = cached {
                    candidates.push(def);
                }
            }
            if candidates.is_empty() {
                if let Some(name) = target_name.as_deref() {
                    let cached = match def_cache_by_name.get(name) {
                        Some(defs) => defs.clone(),
                        None => {
                            let defs = db_definitions_for_name(conn, name, None)?;
                            def_cache_by_name.insert(name.to_string(), defs.clone());
                            defs
                        }
                    };
                    candidates.extend(cached);
                }
            }
            for def in candidates {
                if seen.insert(def.qualified_name.clone()) {
                    defs.push(def);
                    if defs.len() >= limit {
                        return Ok(defs);
                    }
                }
            }
        }
    }
    Ok(defs)
}

fn db_class_hierarchy(
    conn: &Connection,
    query: &str,
) -> Result<(Vec<Definition>, Vec<Definition>), String> {
    Ok((
        db_direct_hierarchy_defs(conn, "derived_name", "base_name", query, false)?,
        db_direct_hierarchy_defs(conn, "base_name", "derived_name", query, true)?,
    ))
}

fn db_direct_hierarchy_defs(
    conn: &Connection,
    match_col: &str,
    target_col: &str,
    query: &str,
    filter_derived: bool,
) -> Result<Vec<Definition>, String> {
    let names: HashSet<String> = [query.to_string()].into_iter().collect();
    let (condition, values) = name_set_condition(match_col, &names);
    let sql =
        format!("SELECT {match_col}, {target_col} FROM inheritance WHERE {condition} ORDER BY id");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare hierarchy query: {e}"))?;
    let rows = stmt
        .query_map(params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("failed to query hierarchy: {e}"))?;

    let suffix = format!("::{query}");
    let mut defs = Vec::new();
    for row in rows {
        let (matched_name, target_name) =
            row.map_err(|e| format!("failed to read hierarchy row: {e}"))?;
        if filter_derived && matched_name != query && !matched_name.ends_with(&suffix) {
            continue;
        }
        for def in db_definitions_for_name(conn, &target_name, None)? {
            if !filter_derived || def.name == target_name || def.qualified_name == target_name {
                defs.push(def);
            }
        }
    }
    Ok(defs)
}

fn db_aliases_for_seed(conn: &Connection, seed: &str) -> Result<Vec<String>, String> {
    let mut aliases = query_aliases(seed);
    for def in db_definitions_for_name(conn, seed, None)? {
        aliases.push(def.name);
        aliases.push(def.qualified_name);
    }
    aliases.sort();
    aliases.dedup();
    Ok(aliases)
}

fn db_call_edges_for_frontier(
    conn: &Connection,
    frontier: &HashSet<String>,
    direction: &str,
) -> Result<Vec<CallEdge>, String> {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if matches!(direction, "callers" | "both") {
        let (condition, mut condition_values) = name_set_condition("cg.callee_name", frontier);
        clauses.push(condition);
        values.append(&mut condition_values);
    }
    if matches!(direction, "callees" | "both") {
        let (condition, mut condition_values) = name_set_condition("cg.caller_name", frontier);
        clauses.push(condition);
        values.append(&mut condition_values);
    }
    if clauses.is_empty() {
        return Ok(Vec::new());
    }

    let sql = format!(
        "
        SELECT cg.caller_name, cg.callee_name, f.path, cg.line, cg.column
        FROM call_graph cg
        LEFT JOIN files f ON f.id = cg.file_id
        WHERE {}
        ORDER BY cg.id
        ",
        clauses.join(" OR ")
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("failed to prepare call expansion query: {e}"))?;
    let rows = stmt
        .query_map(params_from_iter(values.iter()), row_to_call_edge)
        .map_err(|e| format!("failed to query call expansion: {e}"))?;
    collect_sql_rows(rows, "call expansion")
}

fn row_to_call_edge(row: &Row<'_>) -> rusqlite::Result<CallEdge> {
    Ok(CallEdge {
        caller_name: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
        callee_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        location: Location {
            file: row
                .get::<_, Option<String>>(2)?
                .map(PathBuf::from)
                .unwrap_or_default(),
            line: row.get::<_, i64>(3)? as usize,
            column: row.get::<_, i64>(4)? as usize,
            end_line: 0,
            end_column: 0,
        },
    })
}

fn name_set_condition(column: &str, names: &HashSet<String>) -> (String, Vec<String>) {
    if names.is_empty() {
        return ("0".to_string(), Vec::new());
    }
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();

    let mut clauses = Vec::new();
    let mut values = Vec::new();
    clauses.push(format!(
        "{column} IN ({})",
        vec!["?"; sorted.len()].join(", ")
    ));
    values.extend(sorted.iter().map(|name| (*name).clone()));

    for name in sorted {
        if !name.contains("::") {
            clauses.push(format!("{column} LIKE ? ESCAPE '\\'"));
            values.push(like_qualified_suffix_pattern(name));
        }
    }
    (format!("({})", clauses.join(" OR ")), values)
}

fn query_aliases(query: &str) -> Vec<String> {
    let mut names = vec![query.to_string()];
    if let Some(simple) = simple_name(query) {
        if simple != query {
            names.push(simple.to_string());
        }
    }
    names
}

fn sql_limit(limit: Option<usize>) -> i64 {
    limit.map(|n| n as i64).unwrap_or(-1)
}

fn like_contains_pattern(value: &str) -> String {
    format!("%{}%", escape_like(&value.to_lowercase()))
}

fn like_qualified_suffix_pattern(value: &str) -> String {
    format!("%::{}", escape_like(value))
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn collect_sql_rows<T, F>(rows: rusqlite::MappedRows<'_, F>, label: &str) -> Result<Vec<T>, String>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut values = Vec::new();
    for row in rows {
        values.push(row.map_err(|e| format!("failed to read {label} row: {e}"))?);
    }
    Ok(values)
}

fn ensure_db_parent(db: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(db).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create database directory '{}': {e}",
                    parent.display()
                )
            })?;
        }
    }
    Ok(())
}

fn parse_language(value: Option<&str>) -> Result<Language, String> {
    match value.unwrap_or("cpp").to_ascii_lowercase().as_str() {
        "c" => Ok(Language::C),
        "cpp" | "c++" | "cc" => Ok(Language::Cpp),
        other => Err(format!("unsupported language '{other}'; use c or cpp")),
    }
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::C => "c",
        Language::Cpp => "cpp",
    }
}

fn parse_reference_mode(value: &str) -> Result<ReferenceMode, String> {
    match value.to_ascii_lowercase().as_str() {
        "none" | "off" | "false" | "0" => Ok(ReferenceMode::None),
        "calls" | "call" | "call-sites" => Ok(ReferenceMode::Calls),
        "all" | "identifiers" | "full" => Ok(ReferenceMode::All),
        other => Err(format!(
            "unsupported reference mode '{other}'; use none, calls, or all"
        )),
    }
}

fn owned_definition_hits_with_confidence(
    defs: &[Definition],
    confidence: &str,
    include_snippets: bool,
    radius: usize,
) -> Vec<Value> {
    defs.iter()
        .enumerate()
        .map(|(i, def)| def_hit_with_confidence(def, confidence, i + 1, include_snippets, radius))
        .collect()
}

fn def_hit(
    def: &Definition,
    query: &str,
    rank: usize,
    include_snippets: bool,
    radius: usize,
) -> Value {
    json!({
        "rank": rank,
        "confidence": definition_confidence(def, query),
        "definition": definition_json(def),
        "snippet": include_snippets.then(|| read_snippet(&def.location.file, def.location.line, radius)).flatten(),
    })
}

fn def_hit_with_confidence(
    def: &Definition,
    confidence: &str,
    rank: usize,
    include_snippets: bool,
    radius: usize,
) -> Value {
    json!({
        "rank": rank,
        "confidence": confidence,
        "definition": definition_json(def),
        "snippet": include_snippets.then(|| read_snippet(&def.location.file, def.location.line, radius)).flatten(),
    })
}

fn db_ref_hit(
    reference: &DbReference,
    rank: usize,
    include_snippets: bool,
    radius: usize,
) -> Value {
    json!({
        "rank": rank,
        "confidence": if reference.def_id.is_some() { "resolved_unique_name" } else { "unresolved_name" },
        "reference": db_reference_json(reference),
        "snippet": include_snippets.then(|| read_snippet(&reference.location.file, reference.location.line, radius)).flatten(),
    })
}

fn call_hit(call: &CallEdge, depth: usize, include_snippets: bool, radius: usize) -> Value {
    json!({
        "depth": depth,
        "confidence": "structural_call",
        "call": call_json(call),
        "snippet": include_snippets.then(|| read_snippet(&call.location.file, call.location.line, radius)).flatten(),
    })
}

fn definition_json(def: &Definition) -> Value {
    json!({
        "db_id": def.id,
        "name": def.name,
        "qualified_name": def.qualified_name,
        "kind": def.kind.as_str(),
        "location": location_json(&def.location),
        "parent": def.parent,
        "signature": def.signature,
        "visibility": def.visibility.as_str(),
        "is_definition": def.is_definition,
        "extra": def.extra,
    })
}

fn db_reference_json(reference: &DbReference) -> Value {
    json!({
        "db_id": reference.id,
        "name": reference.name,
        "kind": reference.kind,
        "location": location_json(&reference.location),
        "def_id": reference.def_id,
        "context": reference.context,
    })
}

fn call_json(call: &CallEdge) -> Value {
    json!({
        "caller_name": call.caller_name,
        "callee_name": call.callee_name,
        "location": location_json(&call.location),
    })
}

fn location_json(location: &Location) -> Value {
    json!({
        "file": location.file.to_string_lossy(),
        "line": location.line,
        "column": location.column,
        "end_line": location.end_line,
        "end_column": location.end_column,
    })
}

fn definition_confidence(def: &Definition, query: &str) -> &'static str {
    if def.qualified_name == query {
        "exact_qualified"
    } else if def.name == query {
        "exact_simple"
    } else {
        "substring"
    }
}

fn matches_any(name: &str, aliases: &HashSet<String>) -> bool {
    aliases.contains(name)
        || simple_name(name)
            .map(|simple| aliases.contains(simple))
            .unwrap_or(false)
}

fn edge_key(call: &CallEdge) -> (String, String, String, usize, usize) {
    (
        call.caller_name.clone(),
        call.callee_name.clone(),
        call.location.file.to_string_lossy().to_string(),
        call.location.line,
        call.location.column,
    )
}

fn simple_name(name: &str) -> Option<&str> {
    name.rsplit("::").next()
}

fn read_snippet(path: &Path, line: usize, radius: usize) -> Option<Snippet> {
    if line == 0 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let source = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() || line > lines.len() {
        return None;
    }
    let start = line.saturating_sub(radius).max(1);
    let end = (line + radius).min(lines.len());
    let mut text = String::new();
    for current in start..=end {
        let marker = if current == line { ">" } else { " " };
        text.push_str(&format!("{marker}{current:>6}: {}\n", lines[current - 1]));
    }
    Some(Snippet {
        file: path.to_string_lossy().to_string(),
        focus_line: line,
        start_line: start,
        end_line: end,
        text,
    })
}

fn sql_value_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(n) => json!(n),
        SqlValue::Real(f) => json!(f),
        SqlValue::Text(s) => json!(s),
        SqlValue::Blob(bytes) => json!(bytes),
    }
}

fn emit_json(value: Value, pretty: bool) {
    let rendered = if pretty {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .expect("serialize CLI response");
    println!("{rendered}");
}

fn exit_json_error(message: &str) -> ! {
    emit_json(json!({ "ok": false, "error": message }), false);
    process::exit(2);
}

fn print_help() {
    println!(
        r#"xref-indexer: LLM-first C/C++ xref CLI

Responses are JSON by default. Use --pretty for readable JSON.
Default database: ./.git/code-indexer/xrefs.sqlite3

Commands:
  index --root PATH [--db index.db] [--language cpp|c] [--threads N] [--references calls|all|none]
  reindex --root PATH [--db index.db] [--language cpp|c] [--threads N] [--references calls|all|none]
  bench --root PATH [--db index.db|--save] [--no-save] [--threads N] [--references calls|all|none]
  stats [--db index.db] [--root PATH] [--no-reindex]
  find [--db index.db] SYMBOL [--limit N] [--context N] [--include-search] [--root PATH] [--no-reindex]
  search [--db index.db] PATTERN [--limit N] [--context N] [--root PATH] [--no-reindex]
  refs [--db index.db] SYMBOL [--limit N] [--context N] [--root PATH] [--no-reindex]
  callers [--db index.db] SYMBOL [--limit N] [--context N] [--root PATH] [--no-reindex]
  callees [--db index.db] SYMBOL [--limit N] [--context N] [--root PATH] [--no-reindex]
  hierarchy [--db index.db] CLASS [--context N] [--root PATH] [--no-reindex]
  context --file PATH --line N [--context N]
  context [--db index.db] SYMBOL [--limit N] [--context N] [--root PATH] [--no-reindex]
  expand [--db index.db] SYMBOL [--direction callers|callees|both] [--depth N] [--limit N] [--root PATH] [--no-reindex]
  sql [--db index.db] --sql "SELECT name, qualified_name FROM definitions LIMIT 5" [--root PATH] [--no-reindex]

LLM workflow examples:
  xref-indexer index --root ./tests/fixtures
  xref-indexer reindex --root ./tests/fixtures
  xref-indexer bench --root ./test-data --db /tmp/xrefs-bench.sqlite3 --pretty
  xref-indexer find demo::Derived::run --pretty
  xref-indexer callers compute --pretty
  xref-indexer expand compute --direction callers --depth 2 --pretty

Notes:
  - This is a Tree-sitter structural index, not a compiler oracle.
  - reindex parses only files whose checksums changed since the last saved DB update.
  - DB-backed queries run an incremental reindex check first using saved roots.
  - Pass --no-reindex to query the current DB without checking the filesystem.
  - bench is no-save by default unless --db or --save is provided.
  - Default references mode is none; call_graph is still collected.
  - Use --references calls for call-site refs or --references all for exhaustive identifier refs.
  - refs requires a DB built with --references calls or --references all.
  - Per-reference source context is disabled during indexing unless --reference-context is set.
  - Exact qualified/simple matches are higher confidence than substring matches.
  - Use context/snippet output instead of separate sed -n calls.
"#
    );
}
