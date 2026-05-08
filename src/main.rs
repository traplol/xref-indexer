use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

use rusqlite::types::Value as SqlValue;
use serde::Serialize;
use serde_json::{json, Value};
use xref_indexer::benchmark::{self, BenchmarkConfig};
use xref_indexer::types::{CallEdge, Definition, Location, Reference};
use xref_indexer::{Index, Indexer, Language, ReferenceMode};

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
        match self
            .value("references")
            .or_else(|| self.value("reference-mode"))
            .unwrap_or("calls")
        {
            "none" | "off" | "false" | "0" => Ok(ReferenceMode::None),
            "calls" | "call" | "call-sites" => Ok(ReferenceMode::Calls),
            "all" | "identifiers" | "full" => Ok(ReferenceMode::All),
            other => Err(format!(
                "unsupported reference mode '{other}'; use none, calls, or all"
            )),
        }
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

    let run = builder
        .build()
        .index_with_metrics()
        .map_err(|e| format!("index failed: {e}"))?;
    let db = args.db_path();
    ensure_db_parent(db)?;
    let save_start = Instant::now();
    run.index
        .save_to_db(db)
        .map_err(|e| format!("failed to save database '{db}': {e}"))?;
    let save_ms = save_start.elapsed().as_millis();
    let index_ms = run.metrics.total_ms;

    Ok(json!({
        "ok": true,
        "command": "index",
        "db": db,
        "roots": roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "language": language_name(language),
        "summary": index_summary(&run.index),
        "metrics": run.metrics,
        "save_ms": save_ms,
        "total_ms": index_ms + save_ms,
    }))
}

fn cmd_benchmark(args: &ParsedArgs) -> Result<Value, String> {
    let roots = roots_from_args(args, "benchmark")?;
    let language = parse_language(args.value("language").or_else(|| args.value("lang")))?;
    let db_path = (!args.bool_flag("no-save")).then(|| PathBuf::from(args.db_path()));

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

fn cmd_stats(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    Ok(json!({
        "ok": true,
        "command": "stats",
        "db": db,
        "summary": index_summary(&index),
        "files": index.files(),
    }))
}

fn cmd_find(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let limit = args.limit();
    let radius = args.radius();
    let include_snippets = !args.bool_flag("no-snippets");

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    let exact = index.find_definition(query);
    for def in exact {
        if seen.insert(def.qualified_name.clone()) {
            results.push(def_hit(
                def,
                query,
                results.len() + 1,
                include_snippets,
                radius,
            ));
        }
    }

    if results.is_empty() || args.bool_flag("include-search") {
        for def in index.search_symbols(query) {
            if results.len() >= limit {
                break;
            }
            if seen.insert(def.qualified_name.clone()) {
                results.push(def_hit(
                    def,
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
        "query": query,
        "result_count": results.len(),
        "results": results,
        "notes": ["Tree-sitter structural candidates; prefer exact_qualified/exact_simple before substring results"]
    }))
}

fn cmd_search(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let include_snippets = !args.bool_flag("no-snippets");
    let radius = args.radius();
    let results: Vec<Value> = index
        .search_symbols(query)
        .into_iter()
        .take(args.limit())
        .enumerate()
        .map(|(i, def)| def_hit(def, query, i + 1, include_snippets, radius))
        .collect();

    Ok(json!({
        "ok": true,
        "command": "search",
        "db": db,
        "query": query,
        "result_count": results.len(),
        "results": results,
    }))
}

fn cmd_refs(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let include_snippets = !args.bool_flag("no-snippets");
    let radius = args.radius();
    let mut refs = refs_for_query(&index, query);
    refs.truncate(args.limit());
    let results: Vec<Value> = refs
        .iter()
        .enumerate()
        .map(|(i, r)| ref_hit(r, i + 1, include_snippets, radius))
        .collect();

    Ok(json!({
        "ok": true,
        "command": "refs",
        "db": db,
        "query": query,
        "result_count": results.len(),
        "results": results,
    }))
}

fn cmd_callers(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let defs = call_endpoint_defs(&index, query, true, args.limit());
    Ok(json!({
        "ok": true,
        "command": "callers",
        "db": db,
        "query": query,
        "result_count": defs.len(),
        "results": definition_hits_with_confidence(
            &defs,
            "structural_call_endpoint",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
    }))
}

fn cmd_callees(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let defs = call_endpoint_defs(&index, query, false, args.limit());
    Ok(json!({
        "ok": true,
        "command": "callees",
        "db": db,
        "query": query,
        "result_count": defs.len(),
        "results": definition_hits_with_confidence(
            &defs,
            "structural_call_endpoint",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
    }))
}

fn cmd_hierarchy(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let hierarchy = index.class_hierarchy(query);
    Ok(json!({
        "ok": true,
        "command": "hierarchy",
        "db": db,
        "query": query,
        "bases": definition_hits_with_confidence(
            &hierarchy.bases,
            "inheritance_edge",
            !args.bool_flag("no-snippets"),
            args.radius()
        ),
        "derived": definition_hits_with_confidence(
            &hierarchy.derived,
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
    let index = open_index(db)?;
    let query = args.first_symbol()?;
    let defs = index.find_definition(query);
    let results: Vec<Value> = defs
        .into_iter()
        .take(args.limit())
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
        "query": query,
        "result_count": results.len(),
        "results": results,
    }))
}

fn cmd_expand(args: &ParsedArgs) -> Result<Value, String> {
    let db = args.db_path();
    let index = open_index(db)?;
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

    let mut frontier: HashSet<String> = aliases_for_seed(&index, seed).into_iter().collect();
    let mut visited_symbols = frontier.clone();
    let mut seen_edges = HashSet::new();
    let mut edges = Vec::new();

    for level in 1..=depth {
        let mut next = HashSet::new();
        for call in index.calls() {
            if edges.len() >= limit {
                break;
            }
            if matches!(direction, "callers" | "both") && matches_any(&call.callee_name, &frontier)
            {
                let key = edge_key(call);
                if seen_edges.insert(key) {
                    edges.push(call_hit(call, level, include_snippets, radius));
                    next.insert(call.caller_name.clone());
                }
            }
            if matches!(direction, "callees" | "both") && matches_any(&call.caller_name, &frontier)
            {
                let key = edge_key(call);
                if seen_edges.insert(key) {
                    edges.push(call_hit(call, level, include_snippets, radius));
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

    let conn = rusqlite::Connection::open(db)
        .map_err(|e| format!("failed to open database '{db}': {e}"))?;
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
        "columns": columns,
        "row_count": rows.len(),
        "rows": rows,
    }))
}

fn open_index(db: &str) -> Result<Index, String> {
    let (index, _conn) =
        Index::open_from_db(db).map_err(|e| format!("failed to open database '{db}': {e}"))?;
    Ok(index)
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

fn index_summary(index: &Index) -> Value {
    json!({
        "files": index.files().len(),
        "definitions": index.definition_count(),
        "references": index.reference_count(),
        "calls": index.calls().len(),
        "inheritance_edges": index.inherits().len(),
    })
}

fn definition_hits_with_confidence(
    defs: &[&Definition],
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

fn ref_hit(reference: &Reference, rank: usize, include_snippets: bool, radius: usize) -> Value {
    json!({
        "rank": rank,
        "confidence": if reference.def_id.is_some() { "resolved_unique_name" } else { "unresolved_name" },
        "reference": reference_json(reference),
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

fn reference_json(reference: &Reference) -> Value {
    json!({
        "db_id": reference.id,
        "name": reference.name,
        "kind": reference.kind.as_str(),
        "location": location_json(&reference.location),
        "def_id": reference.def_id.map(|id| id.0),
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

fn refs_for_query<'a>(index: &'a Index, query: &str) -> Vec<&'a Reference> {
    let mut names = vec![query.to_string()];
    if let Some(simple) = simple_name(query) {
        if simple != query {
            names.push(simple.to_string());
        }
    }

    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for name in names {
        for reference in index.find_references(&name) {
            let key = (
                reference.location.file.clone(),
                reference.location.line,
                reference.location.column,
                reference.name.clone(),
            );
            if seen.insert(key) {
                refs.push(reference);
            }
        }
    }
    refs
}

fn call_endpoint_defs<'a>(
    index: &'a Index,
    query: &str,
    callers: bool,
    limit: usize,
) -> Vec<&'a Definition> {
    let mut names = vec![query.to_string()];
    if let Some(simple) = simple_name(query) {
        if simple != query {
            names.push(simple.to_string());
        }
    }

    let mut seen = HashSet::new();
    let mut defs = Vec::new();
    for name in names {
        let found = if callers {
            index.find_callers(&name)
        } else {
            index.find_callees(&name)
        };
        for def in found {
            if seen.insert(def.qualified_name.clone()) {
                defs.push(def);
                if defs.len() >= limit {
                    return defs;
                }
            }
        }
    }
    defs
}

fn aliases_for_seed(index: &Index, seed: &str) -> Vec<String> {
    let mut aliases = vec![seed.to_string()];
    if let Some(simple) = simple_name(seed) {
        aliases.push(simple.to_string());
    }
    for def in index.find_definition(seed) {
        aliases.push(def.name.clone());
        aliases.push(def.qualified_name.clone());
    }
    aliases.sort();
    aliases.dedup();
    aliases
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
  bench --root PATH [--db index.db] [--no-save] [--threads N] [--references calls|all|none]
  stats [--db index.db]
  find [--db index.db] SYMBOL [--limit N] [--context N] [--include-search]
  search [--db index.db] PATTERN [--limit N] [--context N]
  refs [--db index.db] SYMBOL [--limit N] [--context N]
  callers [--db index.db] SYMBOL [--limit N] [--context N]
  callees [--db index.db] SYMBOL [--limit N] [--context N]
  hierarchy [--db index.db] CLASS [--context N]
  context --file PATH --line N [--context N]
  context [--db index.db] SYMBOL [--limit N] [--context N]
  expand [--db index.db] SYMBOL [--direction callers|callees|both] [--depth N] [--limit N]
  sql [--db index.db] --sql "SELECT name, qualified_name FROM definitions LIMIT 5"

LLM workflow examples:
  xref-indexer index --root ./tests/fixtures
  xref-indexer bench --root ./test-data --db /tmp/xrefs-bench.sqlite3 --pretty
  xref-indexer find demo::Derived::run --pretty
  xref-indexer callers compute --pretty
  xref-indexer expand compute --direction callers --depth 2 --pretty

Notes:
  - This is a Tree-sitter structural index, not a compiler oracle.
  - Default references mode is calls; use --references all for exhaustive identifier refs.
  - Per-reference source context is disabled during indexing unless --reference-context is set.
  - Exact qualified/simple matches are higher confidence than substring matches.
  - Use context/snippet output instead of separate sed -n calls.
"#
    );
}
