use std::io::{self, BufRead, Write};

use serde_json::{json, Map, Value};

fn main() {
    if let Err(err) = run() {
        eprintln!("xref-mcp: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("failed to read stdin: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(messages)) => {
                let responses: Vec<Value> = messages.iter().filter_map(handle_message).collect();
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            Ok(message) => handle_message(&message),
            Err(err) => Some(error_response(
                Value::Null,
                -32700,
                format!("parse error: {err}"),
            )),
        };

        if let Some(response) = response {
            write_message(&mut stdout, &response)?;
        }
    }

    Ok(())
}

fn handle_message(message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return id.map(|id| error_response(id, -32600, "missing method"));
    };

    match method {
        "initialize" => id.map(|id| response(id, initialize_result(message.get("params")))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => id.map(|id| response(id, json!({}))),
        "tools/list" => id.map(|id| response(id, json!({ "tools": tools() }))),
        "tools/call" => id.map(|id| response(id, call_tool(message.get("params")))),
        "resources/list" => id.map(|id| response(id, json!({ "resources": [] }))),
        "prompts/list" => id.map(|id| response(id, json!({ "prompts": [] }))),
        _ => id.map(|id| error_response(id, -32601, format!("unknown method '{method}'"))),
    }
}

fn initialize_result(params: Option<&Value>) -> Value {
    let protocol_version = params
        .and_then(|value| value.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("2024-11-05");

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "xref-indexer",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn call_tool(params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return tool_error("tools/call missing params");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_error("tools/call missing tool name");
    };
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match run_tool(name, &arguments) {
        Ok(value) => tool_success(value),
        Err(err) => tool_error(err),
    }
}

fn run_tool(name: &str, args: &Map<String, Value>) -> Result<Value, String> {
    match name {
        "xref_index" => run_index_tool(args, false),
        "xref_reindex" => run_index_tool(args, true),
        "xref_stats" => run_no_symbol_query("stats", args),
        "xref_find" => run_symbol_query("find", args, &["symbol", "name", "query"]),
        "xref_search" => run_symbol_query("search", args, &["pattern", "query", "symbol", "name"]),
        "xref_refs" => run_symbol_query("refs", args, &["symbol", "name", "query"]),
        "xref_callers" => run_symbol_query("callers", args, &["symbol", "name", "query"]),
        "xref_callees" => run_symbol_query("callees", args, &["symbol", "name", "query"]),
        "xref_hierarchy" => {
            run_symbol_query("hierarchy", args, &["class", "symbol", "name", "query"])
        }
        "xref_context" => run_context_tool(args),
        "xref_expand" => run_expand_tool(args),
        "xref_sql" => run_sql_tool(args),
        _ => Err(format!("unknown tool '{name}'")),
    }
}

fn run_index_tool(args: &Map<String, Value>, incremental: bool) -> Result<Value, String> {
    let mut argv = vec![if incremental { "reindex" } else { "index" }.to_string()];
    let roots = roots_arg(args, true)?;
    add_repeated(&mut argv, "--root", &roots);
    add_db(&mut argv, args);
    add_string_flag(&mut argv, args, "--language", &["language", "lang"]);
    add_string_flag(
        &mut argv,
        args,
        "--references",
        &["references", "reference_mode"],
    );
    add_usize_flag(&mut argv, args, "--threads", &["threads"]);
    add_bool_flag(&mut argv, args, "--follow-symlinks", &["follow_symlinks"]);
    add_bool_flag(
        &mut argv,
        args,
        "--reference-context",
        &["reference_context"],
    );
    run_cli(argv)
}

fn run_no_symbol_query(command: &str, args: &Map<String, Value>) -> Result<Value, String> {
    let mut argv = vec![command.to_string()];
    add_query_options(&mut argv, args)?;
    run_cli(argv)
}

fn run_symbol_query(
    command: &str,
    args: &Map<String, Value>,
    symbol_keys: &[&str],
) -> Result<Value, String> {
    let symbol = required_string(args, symbol_keys)?;
    let mut argv = vec![command.to_string(), symbol];
    add_query_options(&mut argv, args)?;
    if command == "find" {
        add_bool_flag(&mut argv, args, "--include-search", &["include_search"]);
    }
    run_cli(argv)
}

fn run_context_tool(args: &Map<String, Value>) -> Result<Value, String> {
    let mut argv = vec!["context".to_string()];
    if let Some(file) = optional_string(args, &["file", "path"]) {
        argv.push("--file".to_string());
        argv.push(file);
        let line = required_usize(args, &["line"])?;
        argv.push("--line".to_string());
        argv.push(line.to_string());
    } else {
        argv.push(required_string(args, &["symbol", "name", "query"])?);
        add_db(&mut argv, args);
        add_roots_optional(&mut argv, args)?;
        add_bool_flag(&mut argv, args, "--no-reindex", &["no_reindex"]);
        add_usize_flag(&mut argv, args, "--limit", &["limit"]);
    }
    add_usize_flag(
        &mut argv,
        args,
        "--context",
        &["context_lines", "context", "radius"],
    );
    run_cli(argv)
}

fn run_expand_tool(args: &Map<String, Value>) -> Result<Value, String> {
    let symbol = required_string(args, &["symbol", "name", "query", "seed"])?;
    let mut argv = vec!["expand".to_string(), symbol];
    add_query_options(&mut argv, args)?;
    add_string_flag(&mut argv, args, "--direction", &["direction"]);
    add_usize_flag(&mut argv, args, "--depth", &["depth"]);
    run_cli(argv)
}

fn run_sql_tool(args: &Map<String, Value>) -> Result<Value, String> {
    let sql = required_string(args, &["sql", "query"])?;
    let mut argv = vec!["sql".to_string(), "--sql".to_string(), sql];
    add_query_options(&mut argv, args)?;
    run_cli(argv)
}

fn add_query_options(argv: &mut Vec<String>, args: &Map<String, Value>) -> Result<(), String> {
    add_db(argv, args);
    add_roots_optional(argv, args)?;
    add_bool_flag(argv, args, "--no-reindex", &["no_reindex"]);
    add_usize_flag(argv, args, "--limit", &["limit"]);
    add_usize_flag(
        argv,
        args,
        "--context",
        &["context_lines", "context", "radius"],
    );
    if bool_arg(args, &["snippets"], true) == Some(false) {
        argv.push("--no-snippets".to_string());
    }
    Ok(())
}

fn run_cli(argv: Vec<String>) -> Result<Value, String> {
    xref_indexer::cli::run_cli_args(&argv)
}

fn add_db(argv: &mut Vec<String>, args: &Map<String, Value>) {
    if let Some(db) = optional_string(args, &["db", "database"]) {
        argv.push("--db".to_string());
        argv.push(db);
    }
}

fn add_roots_optional(argv: &mut Vec<String>, args: &Map<String, Value>) -> Result<(), String> {
    let roots = roots_arg(args, false)?;
    add_repeated(argv, "--root", &roots);
    Ok(())
}

fn add_repeated(argv: &mut Vec<String>, flag: &str, values: &[String]) {
    for value in values {
        argv.push(flag.to_string());
        argv.push(value.clone());
    }
}

fn add_string_flag(argv: &mut Vec<String>, args: &Map<String, Value>, flag: &str, keys: &[&str]) {
    if let Some(value) = optional_string(args, keys) {
        argv.push(flag.to_string());
        argv.push(value);
    }
}

fn add_usize_flag(argv: &mut Vec<String>, args: &Map<String, Value>, flag: &str, keys: &[&str]) {
    if let Some(value) = optional_usize(args, keys) {
        argv.push(flag.to_string());
        argv.push(value.to_string());
    }
}

fn add_bool_flag(argv: &mut Vec<String>, args: &Map<String, Value>, flag: &str, keys: &[&str]) {
    if bool_arg(args, keys, false) == Some(true) {
        argv.push(flag.to_string());
    }
}

fn roots_arg(args: &Map<String, Value>, required: bool) -> Result<Vec<String>, String> {
    let mut roots = Vec::new();
    if let Some(root) = optional_string(args, &["root", "path"]) {
        roots.push(root);
    }
    if let Some(value) = args.get("roots").or_else(|| args.get("paths")) {
        match value {
            Value::Array(items) => {
                for item in items {
                    let Some(root) = item.as_str() else {
                        return Err("roots must contain strings".to_string());
                    };
                    roots.push(root.to_string());
                }
            }
            Value::String(root) => roots.push(root.clone()),
            _ => return Err("roots must be a string or array of strings".to_string()),
        }
    }
    if required && roots.is_empty() {
        return Err("missing required roots array".to_string());
    }
    Ok(roots)
}

fn required_string(args: &Map<String, Value>, keys: &[&str]) -> Result<String, String> {
    optional_string(args, keys).ok_or_else(|| format!("missing required {}", keys.join("/")))
}

fn optional_string(args: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| args.get(*key))
        .find_map(|value| value.as_str().map(str::to_string))
}

fn required_usize(args: &Map<String, Value>, keys: &[&str]) -> Result<usize, String> {
    optional_usize(args, keys).ok_or_else(|| format!("missing required {}", keys.join("/")))
}

fn optional_usize(args: &Map<String, Value>, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .filter_map(|key| args.get(*key))
        .find_map(|value| match value {
            Value::Number(number) => number.as_u64().map(|n| n as usize),
            Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
}

fn bool_arg(args: &Map<String, Value>, keys: &[&str], default: bool) -> Option<bool> {
    for key in keys {
        if let Some(value) = args.get(*key) {
            return match value {
                Value::Bool(value) => Some(*value),
                Value::String(value) => Some(value != "false" && value != "0"),
                _ => None,
            };
        }
    }
    Some(default)
}

fn tool_success(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

fn response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

fn write_message(stdout: &mut io::Stdout, message: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *stdout, message)
        .map_err(|e| format!("failed to encode response: {e}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|e| format!("failed to write response: {e}"))?;
    stdout
        .flush()
        .map_err(|e| format!("failed to flush response: {e}"))
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "xref_index",
            "Build or rebuild the SQLite xref database for one or more C/C++ roots. Use before lookup, or when changing indexing options.",
            json!({
                "type": "object",
                "properties": common_index_properties(),
                "required": ["roots"]
            }),
        ),
        tool(
            "xref_reindex",
            "Incrementally update the SQLite xref database by parsing only new or changed files under the given roots.",
            json!({
                "type": "object",
                "properties": common_index_properties(),
                "required": ["roots"]
            }),
        ),
        tool(
            "xref_find",
            "Find definition candidates for a function, class, method, macro, variable, or qualified C++ symbol. Runs auto-reindex by default.",
            query_schema(&[("symbol", "Symbol name or qualified name to find.")]),
        ),
        tool(
            "xref_search",
            "Substring search over indexed definitions. Use when the exact symbol spelling is unknown.",
            query_schema(&[("pattern", "Case-insensitive substring to search in symbol names.")]),
        ),
        tool(
            "xref_refs",
            "Find stored reference rows for a symbol. Requires an index built with references=calls or references=all.",
            query_schema(&[("symbol", "Symbol name to find references for.")]),
        ),
        tool(
            "xref_callers",
            "Find functions or methods that call the requested symbol using the structural call graph.",
            query_schema(&[("symbol", "Callee symbol name or qualified name.")]),
        ),
        tool(
            "xref_callees",
            "Find functions or methods called by the requested symbol using the structural call graph.",
            query_schema(&[("symbol", "Caller symbol name or qualified name.")]),
        ),
        tool(
            "xref_hierarchy",
            "Find base and derived classes for a class name using indexed inheritance edges.",
            query_schema(&[("class", "Class name or qualified class name.")]),
        ),
        tool(
            "xref_context",
            "Fetch source context by symbol or by explicit file and line.",
            context_schema(),
        ),
        tool(
            "xref_expand",
            "Expand a call graph around a seed symbol across callers, callees, or both.",
            expand_schema(),
        ),
        tool(
            "xref_stats",
            "Show database summary, indexed files, and auto-reindex status.",
            no_symbol_query_schema(),
        ),
        tool(
            "xref_sql",
            "Run a read-only SELECT/WITH/PRAGMA SQL query against the xref SQLite database.",
            sql_schema(),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

fn common_index_properties() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "roots".to_string(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "Source roots to index."
        }),
    );
    properties.insert(
        "db".to_string(),
        json!({
            "type": "string",
            "description": format!("SQLite DB path. Defaults to {}.", xref_indexer::cli::DEFAULT_DB_PATH)
        }),
    );
    properties.insert(
        "language".to_string(),
        json!({
            "type": "string",
            "enum": ["cpp", "c"],
            "description": "Source language. Defaults to cpp."
        }),
    );
    properties.insert(
        "references".to_string(),
        json!({
            "type": "string",
            "enum": ["none", "calls", "all"],
            "description": "Reference storage mode. Default none keeps DB smaller while preserving call_graph."
        }),
    );
    properties.insert(
        "threads".to_string(),
        json!({ "type": "integer", "minimum": 1 }),
    );
    properties.insert("follow_symlinks".to_string(), json!({ "type": "boolean" }));
    properties.insert(
        "reference_context".to_string(),
        json!({
            "type": "boolean",
            "description": "Store per-reference source-line context. Usually leave false and use xref_context lazily."
        }),
    );
    properties
}

fn query_schema(required: &[(&str, &str)]) -> Value {
    let mut properties = common_query_properties();
    for (name, description) in required {
        properties.insert(
            (*name).to_string(),
            json!({
                "type": "string",
                "description": description
            }),
        );
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    })
}

fn no_symbol_query_schema() -> Value {
    json!({
        "type": "object",
        "properties": common_query_properties()
    })
}

fn common_query_properties() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "db".to_string(),
        json!({
            "type": "string",
            "description": format!("SQLite DB path. Defaults to {}.", xref_indexer::cli::DEFAULT_DB_PATH)
        }),
    );
    properties.insert(
        "roots".to_string(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "Optional roots to use for this auto-reindex check instead of saved DB roots."
        }),
    );
    properties.insert(
        "limit".to_string(),
        json!({ "type": "integer", "minimum": 0 }),
    );
    properties.insert(
        "context_lines".to_string(),
        json!({
            "type": "integer",
            "minimum": 0,
            "description": "Source snippet radius around each hit. Defaults to 3."
        }),
    );
    properties.insert(
        "snippets".to_string(),
        json!({
            "type": "boolean",
            "description": "Include source snippets when the underlying command supports them. Defaults to true."
        }),
    );
    properties.insert(
        "no_reindex".to_string(),
        json!({
            "type": "boolean",
            "description": "Skip the query-time incremental reindex check."
        }),
    );
    properties.insert(
        "include_search".to_string(),
        json!({
            "type": "boolean",
            "description": "For xref_find, include substring fallback results even when exact results exist."
        }),
    );
    properties
}

fn context_schema() -> Value {
    let mut properties = common_query_properties();
    properties.insert(
        "symbol".to_string(),
        json!({ "type": "string", "description": "Symbol to fetch context for." }),
    );
    properties.insert(
        "file".to_string(),
        json!({ "type": "string", "description": "Source file path for direct file/line context." }),
    );
    properties.insert(
        "line".to_string(),
        json!({ "type": "integer", "minimum": 1, "description": "1-based source line for direct context." }),
    );
    json!({
        "type": "object",
        "properties": properties
    })
}

fn expand_schema() -> Value {
    let mut properties = common_query_properties();
    properties.insert(
        "symbol".to_string(),
        json!({ "type": "string", "description": "Seed symbol for the call graph expansion." }),
    );
    properties.insert(
        "direction".to_string(),
        json!({
            "type": "string",
            "enum": ["callers", "callees", "both"],
            "description": "Expansion direction. Defaults to both."
        }),
    );
    properties.insert(
        "depth".to_string(),
        json!({ "type": "integer", "minimum": 1, "description": "Expansion depth. Defaults to 1." }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": ["symbol"]
    })
}

fn sql_schema() -> Value {
    let mut properties = common_query_properties();
    properties.insert(
        "sql".to_string(),
        json!({
            "type": "string",
            "description": "Read-only SQL beginning with SELECT, WITH, or PRAGMA."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": ["sql"]
    })
}
