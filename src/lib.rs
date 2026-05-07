mod cpp_parser;
mod index;
mod parser;
pub mod types;

pub mod db {
    //! SQLite persistence for the cross-reference index.
    pub use crate::db_impl::{create_schema, open_from_db, save_to_db};
}
mod db_impl;

pub mod query {
    //! Query methods for the cross-reference index.
    //! These are also available directly on [Index].
    pub use crate::index::ClassHierarchy;
}

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

pub use crate::index::Index;

use crate::cpp_parser::CppParser;
use crate::index::IndexBuilder;
use crate::parser::Parser;

/// Supported languages for indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
}

impl Language {
    fn parser(&self) -> Box<dyn Parser> {
        match self {
            Language::C => Box::new(CppParser::new_c()),
            Language::Cpp => Box::new(CppParser::new_cpp()),
        }
    }

    fn extensions(&self) -> &[&str] {
        match self {
            Language::C => &["c", "h"],
            Language::Cpp => &[
                "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "h", "c",
            ],
        }
    }
}

/// Builds and configures an [Indexer].
pub struct IndexerBuilder {
    paths: Vec<PathBuf>,
    language: Language,
    follow_symlinks: bool,
}

impl IndexerBuilder {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            language: Language::Cpp,
            follow_symlinks: false,
        }
    }

    /// Add a directory to scan for source files.
    pub fn add_directory(mut self, path: impl AsRef<Path>) -> Self {
        self.paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Set the language to parse.
    pub fn language(mut self, lang: Language) -> Self {
        self.language = lang;
        self
    }

    /// Follow symbolic links when scanning directories.
    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.follow_symlinks = follow;
        self
    }

    /// Build the [Indexer].
    pub fn build(self) -> Indexer {
        Indexer {
            paths: self.paths,
            language: self.language,
            follow_symlinks: self.follow_symlinks,
        }
    }
}

/// Indexes a C/C++ codebase and produces an in-memory [Index].
pub struct Indexer {
    paths: Vec<PathBuf>,
    language: Language,
    follow_symlinks: bool,
}

impl Indexer {
    /// Create a new [IndexerBuilder].
    pub fn builder() -> IndexerBuilder {
        IndexerBuilder::new()
    }

    /// Run the indexer over the configured paths. Returns an in-memory [Index].
    pub fn index(&self) -> Result<Index, String> {
        let parser = self.language.parser();
        let extensions = self.language.extensions();
        let mut builder = IndexBuilder::new();
        let mut files_indexed = 0usize;

        for root in &self.paths {
            let walker = WalkDir::new(root)
                .follow_links(self.follow_symlinks)
                .into_iter()
                .filter_entry(|e| {
                    // Skip hidden directories and common build dirs.
                    let name = e.file_name().to_str().unwrap_or("");
                    if name.starts_with('.') && name != "." && name != ".." {
                        return false;
                    }
                    if e.file_type().is_dir() {
                        let skip_dirs = [
                            "build",
                            "cmake-build-debug",
                            "cmake-build-release",
                            "bazel-bin",
                            "bazel-out",
                            "bazel-testlogs",
                            "node_modules",
                            "third_party",
                            "third-party",
                            ".git",
                        ];
                        return !skip_dirs.contains(&name);
                    }
                    true
                });

            for entry in walker {
                let entry = entry.map_err(|e| format!("walk error: {e}"))?;
                let path = entry.path();

                if !path.is_file() {
                    continue;
                }

                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                if !extensions.contains(&ext.as_str()) {
                    continue;
                }

                let source = std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

                let symbols = parser.parse_file(path, &source)?;
                builder.add_file(symbols);
                files_indexed += 1;
            }
        }

        eprintln!("Indexed {files_indexed} files");
        Ok(builder.build())
    }
}

impl Index {
    /// Save this index to a SQLite database file.
    pub fn save_to_db(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<rusqlite::Connection, rusqlite::Error> {
        db::save_to_db(self, path.as_ref())
    }

    /// Load an index from a SQLite database file.
    pub fn open_from_db(path: impl AsRef<Path>) -> rusqlite::Result<(Self, rusqlite::Connection)> {
        db::open_from_db(path.as_ref())
    }
}
