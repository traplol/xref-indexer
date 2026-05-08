pub mod benchmark;
mod cpp_parser;
mod index;
mod parser;
pub mod types;

pub mod db {
    //! SQLite persistence for the cross-reference index.
    pub use crate::db_impl::{
        create_schema, load_index_config, open_from_db, save_index_config, save_to_db, IndexConfig,
    };
}
mod db_impl;

pub mod query {
    //! Query methods for the cross-reference index.
    //! These are also available directly on [Index].
    pub use crate::index::ClassHierarchy;
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use rayon::ThreadPoolBuilder;
use serde::Serialize;
use walkdir::WalkDir;

pub use crate::index::Index;

use crate::cpp_parser::CppParser;
use crate::db_impl::{IncrementalChecksumUpdate, IncrementalFileUpdate};
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

/// Timings and counts from an incremental database reindex.
#[derive(Debug, Clone, Serialize)]
pub struct IncrementalMetrics {
    pub threads: usize,
    pub files_discovered: usize,
    pub files_unchanged: usize,
    pub files_new: usize,
    pub files_modified: usize,
    pub files_removed: usize,
    pub files_indexed: usize,
    pub bytes_scanned: u64,
    pub bytes_read: u64,
    pub lossy_decoded_files: usize,
    pub definitions: usize,
    pub references: usize,
    pub calls: usize,
    pub inheritance_edges: usize,
    pub total_files: usize,
    pub total_definitions: usize,
    pub total_references: usize,
    pub total_calls: usize,
    pub total_inheritance_edges: usize,
    pub discover_ms: u128,
    pub checksum_ms: u128,
    pub parse_ms: u128,
    pub db_update_ms: u128,
    pub total_ms: u128,
}

/// Result of an incremental reindex into an existing SQLite database.
pub struct IncrementalRun {
    pub metrics: IncrementalMetrics,
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

    /// Update a SQLite database by parsing only new or changed files.
    pub fn reindex_db(&self, db_path: impl AsRef<Path>) -> Result<IncrementalRun, String> {
        let paths = self.paths.clone();
        let db_path = db_path.as_ref().to_path_buf();
        let language = self.language;
        let follow_symlinks = self.follow_symlinks;
        let threads = self.threads;
        let reference_mode = self.reference_mode;
        let include_reference_context = self.include_reference_context;

        std::thread::Builder::new()
            .name("xref-indexer".to_string())
            .stack_size(INDEXER_STACK_SIZE)
            .spawn(move || {
                reindex_paths_to_db(
                    paths,
                    db_path,
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

#[derive(Clone)]
struct FileSnapshot {
    path: PathBuf,
    language: String,
    checksum: String,
    bytes: u64,
}

struct DbFileRecord {
    checksum: Option<String>,
    symbols_checksum: Option<String>,
    language: String,
}

fn reindex_paths_to_db(
    paths: Vec<PathBuf>,
    db_path: PathBuf,
    language: Language,
    follow_symlinks: bool,
    threads: Option<usize>,
    reference_mode: ReferenceMode,
    include_reference_context: bool,
) -> Result<IncrementalRun, String> {
    let total_start = Instant::now();
    let discover_start = Instant::now();
    let files = discover_files(&paths, language, follow_symlinks)?;
    let discover_ms = discover_start.elapsed().as_millis();
    let files_discovered = files.len();

    let checksum_start = Instant::now();
    let snapshots = checksum_files(files, language)?;
    let checksum_ms = checksum_start.elapsed().as_millis();
    let bytes_scanned = snapshots.iter().map(|file| file.bytes).sum();

    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create database directory '{}': {e}",
                    parent.display()
                )
            })?;
        }
    }

    let mut conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("failed to open database '{}': {e}", db_path.display()))?;
    crate::db_impl::create_schema(&conn)
        .map_err(|e| format!("failed to prepare database '{}': {e}", db_path.display()))?;
    crate::db_impl::save_index_config(
        &conn,
        &paths,
        language_name(language),
        follow_symlinks,
        reference_mode_name(reference_mode),
        include_reference_context,
    )
    .map_err(|e| format!("failed to save index roots: {e}"))?;

    let db_records = load_db_file_records(&conn)
        .map_err(|e| format!("failed to load database file records: {e}"))?;
    let current_paths: HashSet<PathBuf> = snapshots.iter().map(|file| file.path.clone()).collect();
    let removed_paths: Vec<PathBuf> = db_records
        .keys()
        .filter(|path| !current_paths.contains(*path))
        .cloned()
        .collect();

    let mut files_new = 0;
    let mut files_modified = 0;
    let mut to_parse = Vec::new();
    for snapshot in &snapshots {
        match db_records.get(&snapshot.path) {
            Some(record)
                if record.checksum.as_deref() == Some(snapshot.checksum.as_str())
                    && record.language == snapshot.language => {}
            Some(_) => {
                files_modified += 1;
                to_parse.push(snapshot.clone());
            }
            None => {
                files_new += 1;
                to_parse.push(snapshot.clone());
            }
        }
    }

    let worker_count = worker_count(threads, to_parse.len());
    let parse_start = Instant::now();
    let parsed = parse_files(
        to_parse.iter().map(|file| file.path.clone()).collect(),
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

    let checksums: HashMap<PathBuf, String> = snapshots
        .iter()
        .map(|file| (file.path.clone(), file.checksum.clone()))
        .collect();
    let mut checksum_only_updates = Vec::new();
    let mut symbols_checksums: HashMap<PathBuf, String> = HashMap::new();
    for file in &parsed.files {
        let symbols_checksum = crate::db_impl::file_symbols_checksum(&file.symbols);
        let current_checksum = checksums
            .get(&file.symbols.file)
            .map(String::as_str)
            .unwrap_or("");
        if db_records
            .get(&file.symbols.file)
            .and_then(|record| record.symbols_checksum.as_deref())
            == Some(symbols_checksum.as_str())
        {
            checksum_only_updates.push((
                file.symbols.file.clone(),
                current_checksum.to_string(),
                symbols_checksum,
            ));
        } else {
            symbols_checksums.insert(file.symbols.file.clone(), symbols_checksum);
        }
    }

    let db_update_start = Instant::now();
    if !parsed.files.is_empty() || !removed_paths.is_empty() {
        let checksum_updates: Vec<IncrementalChecksumUpdate<'_>> = checksum_only_updates
            .iter()
            .map(
                |(path, checksum, symbols_checksum)| IncrementalChecksumUpdate {
                    path,
                    checksum,
                    symbols_checksum,
                },
            )
            .collect();
        let updates: Vec<IncrementalFileUpdate<'_>> = parsed
            .files
            .iter()
            .filter(|file| symbols_checksums.contains_key(&file.symbols.file))
            .map(|file| {
                let checksum = checksums
                    .get(&file.symbols.file)
                    .map(String::as_str)
                    .unwrap_or("");
                let symbols_checksum = symbols_checksums
                    .get(&file.symbols.file)
                    .map(String::as_str)
                    .unwrap_or("");
                IncrementalFileUpdate {
                    symbols: &file.symbols,
                    checksum,
                    symbols_checksum,
                }
            })
            .collect();
        crate::db_impl::apply_incremental_update(
            &mut conn,
            &updates,
            &checksum_updates,
            &removed_paths,
        )
        .map_err(|e| format!("failed to apply incremental database update: {e}"))?;
    }
    let db_update_ms = db_update_start.elapsed().as_millis();

    let (total_files, total_definitions, total_references, total_calls, total_inheritance_edges) =
        db_counts(&conn).map_err(|e| format!("failed to read database counts: {e}"))?;

    let metrics = IncrementalMetrics {
        threads: worker_count,
        files_discovered,
        files_unchanged: files_discovered.saturating_sub(files_new + files_modified),
        files_new,
        files_modified,
        files_removed: removed_paths.len(),
        files_indexed,
        bytes_scanned,
        bytes_read,
        lossy_decoded_files,
        definitions,
        references,
        calls,
        inheritance_edges,
        total_files,
        total_definitions,
        total_references,
        total_calls,
        total_inheritance_edges,
        discover_ms,
        checksum_ms,
        parse_ms,
        db_update_ms,
        total_ms: total_start.elapsed().as_millis(),
    };

    eprintln!(
        "Reindexed {} changed/new files in {} ms (discover {} ms, checksum {} ms, parse {} ms, db {} ms, unchanged {}, removed {}, threads {})",
        metrics.files_indexed,
        metrics.total_ms,
        metrics.discover_ms,
        metrics.checksum_ms,
        metrics.parse_ms,
        metrics.db_update_ms,
        metrics.files_unchanged,
        metrics.files_removed,
        metrics.threads
    );
    Ok(IncrementalRun { metrics })
}

fn checksum_files(files: Vec<PathBuf>, language: Language) -> Result<Vec<FileSnapshot>, String> {
    let language = language_name(language).to_string();
    files
        .into_iter()
        .map(|path| {
            let (checksum, bytes) = crate::db_impl::checksum_path(&path)
                .map_err(|e| format!("failed to checksum {}: {e}", path.display()))?;
            Ok(FileSnapshot {
                path,
                language: language.clone(),
                checksum,
                bytes,
            })
        })
        .collect()
}

fn load_db_file_records(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<HashMap<PathBuf, DbFileRecord>> {
    let mut stmt = conn.prepare("SELECT path, language, checksum, symbols_checksum FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            DbFileRecord {
                language: row.get(1)?,
                checksum: row.get(2)?,
                symbols_checksum: row.get(3)?,
            },
        ))
    })?;
    let mut records = HashMap::new();
    for row in rows {
        let (path, record) = row?;
        records.insert(path, record);
    }
    Ok(records)
}

fn db_counts(conn: &rusqlite::Connection) -> rusqlite::Result<(usize, usize, usize, usize, usize)> {
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
            Ok((
                row.get::<_, i64>(0)? as usize,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
                row.get::<_, i64>(3)? as usize,
                row.get::<_, i64>(4)? as usize,
            ))
        },
    )
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::C => "c",
        Language::Cpp => "cpp",
    }
}

fn reference_mode_name(reference_mode: ReferenceMode) -> &'static str {
    match reference_mode {
        ReferenceMode::None => "none",
        ReferenceMode::Calls => "calls",
        ReferenceMode::All => "all",
    }
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
