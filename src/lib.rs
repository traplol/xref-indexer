pub mod benchmark;
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use rayon::ThreadPoolBuilder;
use serde::Serialize;
use walkdir::WalkDir;

pub use crate::index::Index;

use crate::cpp_parser::CppParser;
use crate::index::IndexBuilder;
use crate::parser::Parser;

const INDEXER_STACK_SIZE: usize = 8 * 1024 * 1024;
const PARSE_CHUNK_SIZE: usize = 1;

/// Timings and counts from one indexing pass.
#[derive(Debug, Clone, Serialize)]
pub struct IndexMetrics {
    pub threads: usize,
    pub files_discovered: usize,
    pub files_indexed: usize,
    pub bytes_read: u64,
    pub lossy_decoded_files: usize,
    pub definitions: usize,
    pub references: usize,
    pub calls: usize,
    pub inheritance_edges: usize,
    pub discover_ms: u128,
    pub parse_ms: u128,
    pub build_ms: u128,
    pub total_ms: u128,
}

/// Result of an indexing pass with timing data.
pub struct IndexedRun {
    pub index: Index,
    pub metrics: IndexMetrics,
}

/// How much reference data to collect in addition to definitions and call edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReferenceMode {
    /// Do not populate the refs table. Call graph edges are still collected.
    None,
    /// Populate refs for call targets only. This is the default fast mode.
    Calls,
    /// Populate refs for all identifier-like Tree-sitter nodes.
    All,
}

/// Supported languages for indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
}

impl Language {
    fn parser(
        &self,
        reference_mode: ReferenceMode,
        include_reference_context: bool,
    ) -> Box<dyn Parser> {
        match self {
            Language::C => Box::new(CppParser::new_c_with_options(
                reference_mode,
                include_reference_context,
            )),
            Language::Cpp => Box::new(CppParser::new_cpp_with_options(
                reference_mode,
                include_reference_context,
            )),
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
    threads: Option<usize>,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
}

impl IndexerBuilder {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            language: Language::Cpp,
            follow_symlinks: false,
            threads: None,
            reference_mode: ReferenceMode::Calls,
            include_reference_context: false,
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

    /// Set the number of parser worker threads. `0` means use the default.
    pub fn threads(mut self, threads: usize) -> Self {
        self.threads = (threads > 0).then_some(threads);
        self
    }

    /// Store source-line context on every reference.
    ///
    /// This is disabled by default because snippets can be fetched lazily and
    /// large codebases can contain millions of references.
    pub fn reference_context(mut self, include: bool) -> Self {
        self.include_reference_context = include;
        self
    }

    /// Set how much reference data to collect.
    pub fn reference_mode(mut self, mode: ReferenceMode) -> Self {
        self.reference_mode = mode;
        self
    }

    /// Build the [Indexer].
    pub fn build(self) -> Indexer {
        Indexer {
            paths: self.paths,
            language: self.language,
            follow_symlinks: self.follow_symlinks,
            threads: self.threads,
            reference_mode: self.reference_mode,
            include_reference_context: self.include_reference_context,
        }
    }
}

/// Indexes a C/C++ codebase and produces an in-memory [Index].
pub struct Indexer {
    paths: Vec<PathBuf>,
    language: Language,
    follow_symlinks: bool,
    threads: Option<usize>,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
}

impl Indexer {
    /// Create a new [IndexerBuilder].
    pub fn builder() -> IndexerBuilder {
        IndexerBuilder::new()
    }

    /// Run the indexer over the configured paths. Returns an in-memory [Index].
    pub fn index(&self) -> Result<Index, String> {
        Ok(self.index_with_metrics()?.index)
    }

    /// Run the indexer and return phase timings alongside the in-memory [Index].
    pub fn index_with_metrics(&self) -> Result<IndexedRun, String> {
        let paths = self.paths.clone();
        let language = self.language;
        let follow_symlinks = self.follow_symlinks;
        let threads = self.threads;
        let reference_mode = self.reference_mode;
        let include_reference_context = self.include_reference_context;

        std::thread::Builder::new()
            .name("xref-indexer".to_string())
            .stack_size(INDEXER_STACK_SIZE)
            .spawn(move || {
                index_paths(
                    paths,
                    language,
                    follow_symlinks,
                    threads,
                    reference_mode,
                    include_reference_context,
                )
            })
            .map_err(|e| format!("failed to start indexer thread: {e}"))?
            .join()
            .map_err(|_| "indexer thread panicked".to_string())?
    }
}

fn index_paths(
    paths: Vec<PathBuf>,
    language: Language,
    follow_symlinks: bool,
    threads: Option<usize>,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
) -> Result<IndexedRun, String> {
    let total_start = Instant::now();
    let discover_start = Instant::now();
    let files = discover_files(&paths, language, follow_symlinks)?;
    let discover_ms = discover_start.elapsed().as_millis();
    let files_discovered = files.len();
    let worker_count = worker_count(threads, files_discovered);

    let parse_start = Instant::now();
    let parsed = parse_files(
        files,
        language,
        worker_count,
        reference_mode,
        include_reference_context,
    )?;
    let parse_ms = parse_start.elapsed().as_millis();

    let files_indexed = parsed.files.len();
    let bytes_read = parsed.bytes_read;
    let lossy_decoded_files = parsed.lossy_decoded_files;
    let definitions = parsed
        .files
        .iter()
        .map(|f| f.symbols.definitions.len())
        .sum();
    let references = parsed
        .files
        .iter()
        .map(|f| f.symbols.references.len())
        .sum();
    let calls = parsed.files.iter().map(|f| f.symbols.calls.len()).sum();
    let inheritance_edges = parsed.files.iter().map(|f| f.symbols.inherits.len()).sum();

    let build_start = Instant::now();
    let mut builder = IndexBuilder::with_capacity(
        files_indexed,
        definitions,
        references,
        calls,
        inheritance_edges,
    );
    for file in parsed.files {
        builder.add_file(file.symbols);
    }
    let index = builder.build();
    let build_ms = build_start.elapsed().as_millis();

    let metrics = IndexMetrics {
        threads: worker_count,
        files_discovered,
        files_indexed,
        bytes_read,
        lossy_decoded_files,
        definitions: index.definition_count(),
        references: index.reference_count(),
        calls: index.calls().len(),
        inheritance_edges: index.inherits().len(),
        discover_ms,
        parse_ms,
        build_ms,
        total_ms: total_start.elapsed().as_millis(),
    };

    eprintln!(
        "Indexed {} files in {} ms (discover {} ms, parse {} ms, build {} ms, threads {})",
        metrics.files_indexed,
        metrics.total_ms,
        metrics.discover_ms,
        metrics.parse_ms,
        metrics.build_ms,
        metrics.threads
    );
    Ok(IndexedRun { index, metrics })
}

fn discover_files(
    paths: &[PathBuf],
    language: Language,
    follow_symlinks: bool,
) -> Result<Vec<PathBuf>, String> {
    let extensions = language.extensions();
    let mut files = Vec::new();

    for root in paths {
        let walker = WalkDir::new(root)
            .follow_links(follow_symlinks)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_str().unwrap_or("");
                !should_skip_entry(name, e.file_type().is_dir())
            });

        for entry in walker {
            let entry = entry.map_err(|e| format!("walk error: {e}"))?;
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            if !is_supported_extension(path, extensions) {
                continue;
            }

            files.push(path.to_path_buf());
        }
    }

    Ok(files)
}

fn worker_count(requested: Option<usize>, files: usize) -> usize {
    let default = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    requested.unwrap_or(default).max(1).min(files.max(1))
}

#[derive(Default)]
struct ParsedFiles {
    files: Vec<ParsedFile>,
    bytes_read: u64,
    lossy_decoded_files: usize,
}

struct ParsedFile {
    index: usize,
    symbols: crate::types::FileSymbols,
}

struct SourceFile {
    source: String,
    bytes: usize,
    decoded_lossy: bool,
}

fn parse_files(
    files: Vec<PathBuf>,
    language: Language,
    threads: usize,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
) -> Result<ParsedFiles, String> {
    if files.is_empty() {
        return Ok(ParsedFiles::default());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|worker_id| format!("xref-indexer-worker-{worker_id}"))
        .stack_size(INDEXER_STACK_SIZE)
        .build()
        .map_err(|e| format!("failed to start parser worker pool: {e}"))?;

    let files = Arc::new(files);
    let next_file = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel();

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.scope(|scope| {
            for _ in 0..threads {
                let files = Arc::clone(&files);
                let next_file = Arc::clone(&next_file);
                let sender = sender.clone();
                scope.spawn(move |_| {
                    let result = parse_worker(
                        files,
                        next_file,
                        language,
                        reference_mode,
                        include_reference_context,
                    );
                    let _ = sender.send(result);
                });
            }
        });
    }))
    .map_err(|_| "parser worker panicked".to_string())?;
    drop(sender);

    let mut batches = Vec::with_capacity(threads);
    for _ in 0..threads {
        match receiver.recv() {
            Ok(Ok(batch)) => batches.push(batch),
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(format!("parser worker did not report results: {e}")),
        }
    }

    let mut parsed = ParsedFiles {
        files: Vec::with_capacity(batches.iter().map(|batch| batch.files.len()).sum()),
        bytes_read: batches.iter().map(|batch| batch.bytes_read).sum(),
        lossy_decoded_files: batches.iter().map(|batch| batch.lossy_decoded_files).sum(),
    };

    for batch in batches {
        parsed.files.extend(batch.files);
    }
    parsed.files.sort_by_key(|file| file.index);
    Ok(parsed)
}

fn parse_worker(
    files: Arc<Vec<PathBuf>>,
    next_file: Arc<AtomicUsize>,
    language: Language,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
) -> Result<ParsedFiles, String> {
    let mut parser = language.parser(reference_mode, include_reference_context);
    let mut parsed = ParsedFiles::default();

    loop {
        let start = next_file.fetch_add(PARSE_CHUNK_SIZE, Ordering::Relaxed);
        if start >= files.len() {
            break;
        }
        let end = (start + PARSE_CHUNK_SIZE).min(files.len());

        for index in start..end {
            let path = &files[index];
            let source = read_source_lossy(path)?;
            parsed.bytes_read += source.bytes as u64;
            parsed.lossy_decoded_files += usize::from(source.decoded_lossy);
            let symbols = parser
                .parse_file(path, &source.source)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
            parsed.files.push(ParsedFile { index, symbols });
        }
    }

    Ok(parsed)
}

fn should_skip_entry(name: &str, is_dir: bool) -> bool {
    if name.starts_with('.') && name != "." && name != ".." {
        return true;
    }
    if !is_dir {
        return false;
    }
    matches!(
        name,
        "build"
            | "cmake-build-debug"
            | "cmake-build-release"
            | "bazel-bin"
            | "bazel-out"
            | "bazel-testlogs"
            | "node_modules"
            | "third_party"
            | "third-party"
            | ".git"
    )
}

fn is_supported_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            extensions
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
        .unwrap_or(false)
}

fn read_source_lossy(path: &std::path::Path) -> Result<SourceFile, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let byte_len = bytes.len();
    match String::from_utf8(bytes) {
        Ok(source) => Ok(SourceFile {
            source,
            bytes: byte_len,
            decoded_lossy: false,
        }),
        Err(err) => {
            eprintln!(
                "Warning: decoded {} with replacement characters because it is not valid UTF-8",
                path.display()
            );
            Ok(SourceFile {
                source: String::from_utf8_lossy(err.as_bytes()).into_owned(),
                bytes: byte_len,
                decoded_lossy: true,
            })
        }
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
