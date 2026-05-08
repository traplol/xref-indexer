use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;

use xref_indexer::types::*;
use xref_indexer::Index;
use xref_indexer::Indexer;

// ── CLI args ──

#[derive(Debug)]
struct Args {
    data_dir: PathBuf,
    host: String,
    port: u16,
}

fn parse_args() -> Args {
    let mut data_dir = PathBuf::from("./data");
    let mut host = "127.0.0.1".to_string();
    let mut port = 8970u16;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                if let Some(dir) = args.next() {
                    data_dir = PathBuf::from(dir);
                }
            }
            "--host" => {
                if let Some(h) = args.next() {
                    host = h;
                }
            }
            "--port" => {
                if let Some(p) = args.next() {
                    port = p.parse().unwrap_or(port);
                }
            }
            _ => {}
        }
    }
    Args {
        data_dir,
        host,
        port,
    }
}

// ── App state ──

struct AppState {
    data_dir: PathBuf,
}

// ── Request body types ──

#[derive(Deserialize)]
struct IndexRequest {
    path: String,
    language: String,
    db: String,
}

#[derive(Deserialize)]
struct QueryRequest {
    sql: String,
}

#[derive(Deserialize)]
struct NameQuery {
    name: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct ClassQuery {
    #[serde(rename = "class")]
    class_name: String,
}

// ── Helpers ──

fn db_path(state: &AppState, name: &str) -> PathBuf {
    let safe_name = name.replace('/', "_");
    state.data_dir.join(format!("{safe_name}.db"))
}

fn open_index(state: &AppState, name: &str) -> Result<(Index, rusqlite::Connection), String> {
    let path = db_path(state, name);
    if !path.exists() {
        return Err(format!("database '{name}' not found at {}", path.display()));
    }
    Index::open_from_db(&path).map_err(|e| format!("failed to open DB: {e}"))
}

fn json_ok(data: Value) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({ "success": true, "data": data })),
    )
}

fn json_err(status: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "success": false, "error": msg })))
}

// ── Handlers ──

async fn handle_index(
    State(state): State<Arc<AppState>>,
    Json(req): Json<IndexRequest>,
) -> (StatusCode, Json<Value>) {
    let lang = match req.language.to_lowercase().as_str() {
        "c" => xref_indexer::Language::C,
        "cpp" | "c++" => xref_indexer::Language::Cpp,
        other => {
            return json_err(
                StatusCode::BAD_REQUEST,
                &format!("unsupported language: '{other}' (use 'c' or 'cpp')"),
            );
        }
    };

    let indexer = Indexer::builder()
        .add_directory(&req.path)
        .language(lang)
        .build();

    let index = match indexer.index() {
        Ok(idx) => idx,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("index failed: {e}"),
            )
        }
    };

    let db_path = db_path(&state, &req.db);
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Err(e) = index.save_to_db(&db_path) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("save failed: {e}"),
        );
    }

    json_ok(json!({
        "db": req.db,
        "path": db_path.to_string_lossy(),
        "definitions": index.definition_count(),
        "references": index.reference_count(),
        "files": index.files().len(),
    }))
}

async fn handle_definitions(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<NameQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let defs: Vec<Definition> = index
        .find_definition(&q.name)
        .into_iter()
        .cloned()
        .collect();
    json_ok(json!(defs))
}

async fn handle_references(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<NameQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let refs: Vec<Reference> = index
        .find_references(&q.name)
        .into_iter()
        .cloned()
        .collect();
    json_ok(json!(refs))
}

async fn handle_callers(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<NameQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let callers: Vec<Definition> = index.find_callers(&q.name).into_iter().cloned().collect();
    json_ok(json!(callers))
}

async fn handle_callees(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<NameQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let callees: Vec<Definition> = index.find_callees(&q.name).into_iter().cloned().collect();
    json_ok(json!(callees))
}

async fn handle_search(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<SearchQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let results: Vec<Definition> = index.search_symbols(&q.q).into_iter().cloned().collect();
    json_ok(json!(results))
}

async fn handle_hierarchy(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Query(q): Query<ClassQuery>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let hierarchy = index.class_hierarchy(&q.class_name);
    let bases: Vec<Definition> = hierarchy.bases.into_iter().cloned().collect();
    let derived: Vec<Definition> = hierarchy.derived.into_iter().cloned().collect();
    json_ok(json!({ "bases": bases, "derived": derived }))
}

async fn handle_stats(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
) -> (StatusCode, Json<Value>) {
    let (index, _conn) = match open_index(&state, &dbname) {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::NOT_FOUND, &e),
    };

    let files: Vec<IndexedFile> = index.files().to_vec();
    json_ok(json!({
        "definitions": index.definition_count(),
        "references": index.reference_count(),
        "calls": index.calls().len(),
        "inherits": index.inherits().len(),
        "files": files,
    }))
}

async fn handle_query(
    State(state): State<Arc<AppState>>,
    Path(dbname): Path<String>,
    Json(req): Json<QueryRequest>,
) -> (StatusCode, Json<Value>) {
    let path = db_path(&state, &dbname);
    if !path.exists() {
        return json_err(
            StatusCode::NOT_FOUND,
            &format!("database '{dbname}' not found"),
        );
    }

    let conn = match rusqlite::Connection::open(&path) {
        Ok(c) => c,
        Err(e) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to open DB: {e}"),
            )
        }
    };

    let mut stmt = match conn.prepare(&req.sql) {
        Ok(s) => s,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, &format!("invalid SQL: {e}")),
    };

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();

    let rows: Vec<Value> = match stmt.query_map([], |row| {
        let mut map = serde_json::Map::new();
        for i in 0..col_count {
            let name = &col_names[i];
            let val: rusqlite::types::Value = row.get(i).unwrap_or(rusqlite::types::Value::Null);
            let jv = match val {
                rusqlite::types::Value::Null => Value::Null,
                rusqlite::types::Value::Integer(n) => json!(n),
                rusqlite::types::Value::Real(f) => json!(f),
                rusqlite::types::Value::Text(s) => json!(s),
                rusqlite::types::Value::Blob(b) => json!(b),
            };
            map.insert(name.clone(), jv);
        }
        Ok(Value::Object(map))
    }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(e) => return json_err(StatusCode::BAD_REQUEST, &format!("query error: {e}")),
    };

    json_ok(json!({ "columns": col_names, "rows": rows }))
}

async fn handle_webapp() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}

async fn handle_list_dbs(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let mut dbs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&state.data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "db") {
                if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                    dbs.push(json!({
                        "name": name,
                        "path": path.to_string_lossy(),
                    }));
                }
            }
        }
    }
    json_ok(json!(dbs))
}

// ── Main ──

#[tokio::main]
async fn main() {
    let args = parse_args();

    if !args.data_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&args.data_dir) {
            eprintln!(
                "Error: failed to create data directory {:?}: {e}",
                args.data_dir
            );
            std::process::exit(1);
        }
    }

    let state = Arc::new(AppState {
        data_dir: args.data_dir.clone(),
    });

    let app = Router::new()
        .route("/", get(handle_webapp))
        .route("/api/v1/dbs", get(handle_list_dbs))
        .route("/api/v1/index", post(handle_index))
        .route("/api/v1/db/{dbname}/definitions", get(handle_definitions))
        .route("/api/v1/db/{dbname}/references", get(handle_references))
        .route("/api/v1/db/{dbname}/callers", get(handle_callers))
        .route("/api/v1/db/{dbname}/callees", get(handle_callees))
        .route("/api/v1/db/{dbname}/search", get(handle_search))
        .route("/api/v1/db/{dbname}/hierarchy", get(handle_hierarchy))
        .route("/api/v1/db/{dbname}/stats", get(handle_stats))
        .route("/api/v1/db/{dbname}/query", post(handle_query))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    eprintln!("xref-indexer server listening on http://{addr}");
    eprintln!("  data directory: {:?}", args.data_dir);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to bind to {addr}: {e}");
            std::process::exit(1);
        });

    axum::serve(listener, app).await.unwrap_or_else(|e| {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    });
}
