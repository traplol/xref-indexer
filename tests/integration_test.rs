use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use xref_indexer::benchmark::{self, BenchmarkConfig};
use xref_indexer::types::{SymbolKind, Visibility};
use xref_indexer::{Indexer, Language, ReferenceMode};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_index() -> xref_indexer::Index {
    Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .reference_mode(ReferenceMode::All)
        .build()
        .index()
        .expect("indexing should succeed")
}

fn unique_db_path(prefix: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}.sqlite3", std::process::id()))
}

fn run_cli_json(args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_xref-indexer"))
        .args(args)
        .output()
        .expect("run xref-indexer CLI");
    assert!(
        output.status.success(),
        "CLI failed with status {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI should emit JSON")
}

#[test]
fn test_index_cpp_fixtures() {
    let indexer = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .reference_mode(ReferenceMode::All)
        .build();

    let index = indexer.index().expect("indexing should succeed");

    // ── Definitions ──
    let all_defs: Vec<_> = index.definitions().iter().collect();

    // Check that key symbols are found.
    let def_names: Vec<&str> = all_defs.iter().map(|d| d.qualified_name.as_str()).collect();

    // Macro
    assert!(
        def_names.contains(&"MAX_SIZE"),
        "expected MAX_SIZE macro; got: {def_names:?}"
    );

    // Namespace should be found somewhere (either "demo" or "demo::demo")
    let has_namespace = def_names
        .iter()
        .any(|n| *n == "demo" || n.ends_with("::demo") || n.starts_with("demo::"));
    assert!(has_namespace, "expected demo namespace; got: {def_names:?}");

    // Class — check for Base and Derived qualified by demo namespace
    let has_base = def_names.iter().any(|n| n.contains("Base"));
    let has_derived = def_names.iter().any(|n| n.contains("Derived"));
    assert!(has_base, "expected Base class; got: {def_names:?}");
    assert!(has_derived, "expected Derived class; got: {def_names:?}");

    // Enums & variants
    let has_color = def_names
        .iter()
        .any(|n| n.ends_with("::Color") || *n == "Color");
    assert!(has_color, "expected Color enum; got: {def_names:?}");
    for variant in &["RED", "GREEN", "BLUE"] {
        let found = all_defs
            .iter()
            .any(|d| d.name == *variant && d.kind == SymbolKind::EnumVariant);
        assert!(found, "expected enum variant {variant}");
    }

    // Global variable
    let has_global = def_names
        .iter()
        .any(|n| n.ends_with("::global_counter") || *n == "global_counter");
    assert!(has_global, "expected global_counter; got: {def_names:?}");

    // Function
    let has_compute = def_names
        .iter()
        .any(|n| n.ends_with("::compute") || *n == "compute");
    assert!(has_compute, "expected compute function; got: {def_names:?}");

    // Class method (at least the definition in demo.cpp)
    let run_defs: Vec<_> = all_defs.iter().filter(|d| d.name == "run").collect();
    assert!(!run_defs.is_empty(), "expected run method(s)");

    // Constructor
    let ctor_defs: Vec<_> = all_defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Constructor)
        .collect();
    assert!(!ctor_defs.is_empty(), "expected constructor(s)");

    // ── Inheritance ──
    let inherits = index.inherits();
    let derived_from_base = inherits
        .iter()
        .any(|inh| inh.derived_name == "Derived" && inh.base_name == "Base");
    assert!(
        derived_from_base,
        "expected Derived -> Base inheritance; got: {inherits:?}"
    );

    // ── Call graph ──
    let calls = index.calls();
    assert!(!calls.is_empty(), "expected some call edges");

    // ── References ──
    let refs = index.references();
    assert!(!refs.is_empty(), "expected some references");

    // Verify reference resolution (unambiguous names should resolve)
    let resolved_refs: Vec<_> = refs.iter().filter(|r| r.def_id.is_some()).collect();
    // At least some refs should be resolved
    // (compute, global_counter, etc. are unique enough)
    eprintln!(
        "Stats: {} defs, {} refs ({} resolved), {} calls, {} inherits",
        all_defs.len(),
        refs.len(),
        resolved_refs.len(),
        calls.len(),
        inherits.len(),
    );

    // Check that we found a reasonable number of definitions
    assert!(
        all_defs.len() >= 15,
        "expected at least 15 definitions, got {}",
        all_defs.len()
    );
    assert!(
        refs.len() >= 10,
        "expected at least 10 references, got {}",
        refs.len()
    );
}

#[test]
fn test_search_symbols() {
    let indexer = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .reference_mode(ReferenceMode::All)
        .build();

    let index = indexer.index().expect("indexing should succeed");

    let results = index.search_symbols("compute");
    assert!(
        !results.is_empty(),
        "search for 'compute' should return results"
    );
    assert!(results.iter().any(|d| d.name == "compute"));

    let results = index.search_symbols("nonexistent_xyzzy");
    assert!(results.is_empty());
}

#[test]
fn test_indexer_decodes_invalid_utf8_lossily() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "xref-indexer-invalid-utf8-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp fixture dir");
    std::fs::write(
        root.join("invalid.cpp"),
        b"int valid_symbol() { return 1; }\n// legacy byte: \xFF\nint caller() { return valid_symbol(); }\n",
    )
    .expect("write invalid UTF-8 fixture");

    let index = Indexer::builder()
        .add_directory(&root)
        .language(Language::Cpp)
        .build()
        .index()
        .expect("indexing should tolerate invalid UTF-8");

    let def_names: Vec<_> = index
        .definitions()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        def_names.contains(&"valid_symbol"),
        "expected valid_symbol definition; got: {def_names:?}"
    );
    assert!(
        def_names.contains(&"caller"),
        "expected caller definition; got: {def_names:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn test_call_graph_uses_function_callers_and_keeps_call_references() {
    let index = fixture_index();

    assert!(
        index
            .calls()
            .iter()
            .any(|c| c.caller_name == "demo::Derived::run" && c.callee_name == "compute"),
        "expected Derived::run -> compute call; got: {:?}",
        index.calls()
    );
    assert!(
        index
            .calls()
            .iter()
            .any(|c| c.caller_name == "main" && c.callee_name == "compute"),
        "expected main -> compute call; got: {:?}",
        index.calls()
    );

    let callers: Vec<_> = index
        .find_callers("compute")
        .iter()
        .map(|d| d.qualified_name.as_str())
        .collect();
    assert!(
        callers.contains(&"demo::Derived::run"),
        "expected compute caller demo::Derived::run; got: {callers:?}"
    );
    assert!(
        callers.contains(&"main"),
        "expected compute caller main; got: {callers:?}"
    );

    let callees: Vec<_> = index
        .find_callees("demo::Derived::run")
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        callees.contains(&"compute"),
        "expected demo::Derived::run callee compute; got: {callees:?}"
    );

    let compute_ref_lines: Vec<_> = index
        .find_references("compute")
        .iter()
        .map(|r| r.location.line)
        .collect();
    assert!(
        compute_ref_lines.contains(&15) && compute_ref_lines.contains(&25),
        "expected compute call references on lines 15 and 25; got: {compute_ref_lines:?}"
    );
}

#[test]
fn test_reference_modes_control_reference_volume() {
    let calls_only = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .reference_mode(ReferenceMode::Calls)
        .build()
        .index()
        .expect("calls-only indexing should succeed");
    let no_refs = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .reference_mode(ReferenceMode::None)
        .build()
        .index()
        .expect("no-ref indexing should succeed");
    let all_refs = fixture_index();

    assert!(
        calls_only.reference_count() > 0,
        "calls mode should keep call-site refs"
    );
    assert_eq!(
        no_refs.reference_count(),
        0,
        "none mode should suppress refs"
    );
    assert!(
        all_refs.reference_count() > calls_only.reference_count(),
        "all mode should collect more refs than calls mode"
    );
    assert_eq!(
        all_refs.calls().len(),
        calls_only.calls().len(),
        "reference mode should not change call graph edges"
    );
    assert_eq!(
        calls_only.calls().len(),
        no_refs.calls().len(),
        "none mode should still collect call graph edges"
    );
}

#[test]
fn test_scoped_methods_constructors_signatures_and_inheritance() {
    let index = fixture_index();
    let definitions = index.definitions();

    assert!(
        definitions.iter().any(|d| {
            d.qualified_name == "demo::Derived::run"
                && d.kind == SymbolKind::Method
                && d.is_definition
        }),
        "expected out-of-class method definition demo::Derived::run; got: {:?}",
        definitions
            .iter()
            .filter(|d| d.name == "run")
            .collect::<Vec<_>>()
    );
    assert!(
        !definitions.iter().any(|d| d.qualified_name == "demo::run"),
        "out-of-class method should not be indexed as demo::run"
    );
    assert!(
        definitions.iter().any(|d| {
            d.qualified_name == "demo::Base::Base" && d.kind == SymbolKind::Constructor
        }),
        "expected inline constructor demo::Base::Base"
    );
    assert!(
        !definitions
            .iter()
            .any(|d| d.qualified_name == "demo::Derived::run" && d.kind == SymbolKind::Constructor),
        "ordinary method run must not be classified as a constructor"
    );

    let compute_def = definitions
        .iter()
        .find(|d| d.name == "compute" && d.is_definition)
        .expect("expected compute definition");
    let signature = compute_def
        .signature
        .as_ref()
        .expect("compute definition should have a signature");
    assert!(
        signature.contains("int compute(int x, int y)") && !signature.contains("return"),
        "signature should contain the function header but not the body; got: {signature:?}"
    );

    let inheritance = index
        .inherits()
        .iter()
        .find(|inh| inh.derived_name == "Derived" && inh.base_name == "Base")
        .expect("expected Derived -> Base inheritance");
    assert_eq!(
        inheritance.access,
        Visibility::Public,
        "class Derived : public Base should be public"
    );
}

#[test]
fn test_create_schema_adds_parent_name_to_legacy_schema() {
    let db_path = fixtures_dir().join("legacy_schema.db");
    let _ = std::fs::remove_file(&db_path);

    {
        let conn = rusqlite::Connection::open(&db_path).expect("open legacy db");
        conn.execute_batch(
            "
            CREATE TABLE definitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file_id INTEGER,
                line INTEGER NOT NULL,
                column INTEGER NOT NULL,
                end_line INTEGER NOT NULL DEFAULT 0,
                end_column INTEGER NOT NULL DEFAULT 0,
                parent_id INTEGER REFERENCES definitions(id),
                signature TEXT,
                visibility TEXT NOT NULL DEFAULT 'public',
                is_definition INTEGER NOT NULL DEFAULT 1,
                extra TEXT
            );
            ",
        )
        .expect("create legacy definitions table");
    }

    let conn = rusqlite::Connection::open(&db_path).expect("reopen legacy db");
    xref_indexer::db::create_schema(&conn).expect("schema migration should succeed");
    let mut stmt = conn
        .prepare("PRAGMA table_info(definitions)")
        .expect("read table info");
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get(1))
        .expect("query table info")
        .map(|row| row.expect("column name"))
        .collect();
    assert!(
        columns.iter().any(|c| c == "parent_name"),
        "expected parent_name in migrated schema; got: {columns:?}"
    );

    drop(stmt);
    drop(conn);
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_save_and_load_db() {
    let indexer = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
        .build();

    let index = indexer.index().expect("indexing should succeed");

    let db_path = fixtures_dir().join("test_output.db");
    let _conn = index.save_to_db(&db_path).expect("save should succeed");

    let (loaded, _conn) = xref_indexer::Index::open_from_db(&db_path).expect("load should succeed");

    assert_eq!(
        loaded.definition_count(),
        index.definition_count(),
        "round-trip should preserve definition count"
    );
    assert_eq!(
        loaded.reference_count(),
        index.reference_count(),
        "round-trip should preserve reference count"
    );

    // Clean up.
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_cli_queries_saved_sqlite_db() {
    let index = fixture_index();
    let db_path = unique_db_path("xref-indexer-cli-query");
    let conn = index.save_to_db(&db_path).expect("save should succeed");
    drop(conn);

    let db = db_path.to_string_lossy().to_string();
    let stats = run_cli_json(&["stats", "--db", &db]);
    assert_eq!(stats["ok"], true);
    assert_eq!(
        stats["summary"]["definitions"].as_u64(),
        Some(index.definition_count() as u64)
    );

    let find = run_cli_json(&[
        "find",
        "--db",
        &db,
        "demo::Derived::run",
        "--limit",
        "5",
        "--no-snippets",
    ]);
    assert_eq!(find["ok"], true);
    assert_eq!(find["results"][0]["confidence"], "exact_qualified");
    assert_eq!(
        find["results"][0]["definition"]["qualified_name"],
        "demo::Derived::run"
    );

    let callers = run_cli_json(&[
        "callers",
        "--db",
        &db,
        "compute",
        "--limit",
        "10",
        "--no-snippets",
    ]);
    let caller_names: Vec<_> = callers["results"]
        .as_array()
        .expect("callers results should be an array")
        .iter()
        .filter_map(|hit| hit["definition"]["qualified_name"].as_str())
        .collect();
    assert!(
        caller_names.contains(&"demo::Derived::run"),
        "expected Derived::run caller; got {caller_names:?}"
    );
    assert!(
        caller_names.contains(&"main"),
        "expected main caller; got {caller_names:?}"
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_benchmark_run_reports_metrics_without_save() {
    let report = benchmark::run(BenchmarkConfig {
        roots: vec![fixtures_dir()],
        language: Language::Cpp,
        reference_mode: ReferenceMode::Calls,
        db_path: None,
        ..BenchmarkConfig::default()
    })
    .expect("benchmark should succeed");

    assert!(
        report.save.is_none(),
        "no-save benchmark should not save DB"
    );
    assert_eq!(report.index.files_indexed, 2);
    assert!(report.index.definitions >= 15);
    assert!(report.index.calls > 0);
    assert!(report.total_ms >= report.index.total_ms);
}
