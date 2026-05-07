use std::path::PathBuf;

use xref_indexer::types::SymbolKind;
use xref_indexer::{Indexer, Language};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn test_index_cpp_fixtures() {
    let indexer = Indexer::builder()
        .add_directory(fixtures_dir())
        .language(Language::Cpp)
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
