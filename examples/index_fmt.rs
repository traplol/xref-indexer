use std::path::PathBuf;

use xref_indexer::{Indexer, Language};

fn main() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data/fmt");

    let indexer = Indexer::builder()
        .add_directory(&repo)
        .language(Language::Cpp)
        .build();

    let index = indexer.index().expect("indexing failed");

    println!("=== fmtlib/fmt ===");
    println!("Definitions: {}", index.definition_count());
    println!("References:   {}", index.reference_count());
    println!("Calls:        {}", index.calls().len());
    println!("Inherits:     {}", index.inherits().len());

    // Show top 10 most-referenced symbols.
    let mut def_refs: Vec<(&str, usize)> = index
        .definitions()
        .iter()
        .map(|d| {
            let count = index.find_references(&d.name).len();
            (d.qualified_name.as_str(), count)
        })
        .collect();
    def_refs.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\nTop 10 most-referenced symbols:");
    for (name, count) in def_refs.iter().take(10) {
        println!("  {name}: {count} refs");
    }

    // Show class hierarchy if any.
    for inh in index.inherits() {
        println!("\nInheritance: {} -> {}", inh.derived_name, inh.base_name);
    }

    // Search for a few well-known symbols.
    for name in &["format", "format_to", "vformat", "FMT_COMPILE"] {
        let defs = index.find_definition(name);
        if !defs.is_empty() {
            println!("\n'{name}': {} definition(s)", defs.len());
            for d in &defs {
                println!("  {} {} at {}:{}", d.kind.as_str(), d.qualified_name, d.location.file.display(), d.location.line);
            }
        }
    }
}
