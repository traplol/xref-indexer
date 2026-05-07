use std::path::PathBuf;

use xref_indexer::{Indexer, Language};

fn main() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data/json");

    let indexer = Indexer::builder()
        .add_directory(&repo)
        .language(Language::Cpp)
        .build();

    let index = indexer.index().expect("indexing failed");

    println!("=== nlohmann/json ===");
    println!("Definitions: {}", index.definition_count());
    println!("References:   {}", index.reference_count());
    println!("Calls:        {}", index.calls().len());
    println!("Inherits:     {}", index.inherits().len());

    // Search for well-known symbols.
    let searches = ["basic_json", "json", "to_json", "from_json", "NLOHMANN_DEFINE_TYPE_INTRUSIVE"];
    for name in &searches {
        let defs = index.find_definition(name);
        println!("\n'{name}': {} definition(s)", defs.len());
        for d in defs.iter().take(3) {
            println!("  {} {} at {}:{}", d.kind.as_str(), d.qualified_name, d.location.file.display(), d.location.line);
        }
        if defs.len() > 3 {
            println!("  ... and {} more", defs.len() - 3);
        }
    }

    // Symbols defined in JSON's own namespace.
    let json_syms: Vec<_> = index
        .definitions()
        .iter()
        .filter(|d| d.qualified_name.starts_with("nlohmann::") || d.qualified_name == "nlohmann")
        .take(20)
        .map(|d| &d.name)
        .collect();
    println!("\nSample nlohmann:: symbols: {:?}", json_syms);
}
