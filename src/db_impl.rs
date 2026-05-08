use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use rusqlite::{params, Connection};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use crate::index::Index;
use crate::types::*;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub struct IncrementalFileUpdate<'a> {
    pub symbols: &'a FileSymbols,
    pub checksum: &'a str,
    pub symbols_checksum: &'a str,
}

pub struct IncrementalChecksumUpdate<'a> {
    pub path: &'a Path,
    pub checksum: &'a str,
    pub symbols_checksum: &'a str,
}

pub struct StreamedFileSymbols {
    pub index: usize,
    pub symbols: FileSymbols,
    pub checksum: String,
    pub symbols_checksum: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamedDbWriteMetrics {
    pub setup_ms: u128,
    pub streamed_files_and_definitions_ms: u128,
    pub pending_edges_ms: u128,
    pub commit_ms: u128,
    pub create_indexes_ms: u128,
    pub finalize_ms: u128,
    pub total_ms: u128,
}

struct PendingReference {
    name: String,
    kind: SymbolKind,
    file_id: i64,
    line: usize,
    column: usize,
    context: Option<String>,
}

struct PendingCall {
    caller_name: String,
    callee_name: String,
    file_id: i64,
    line: usize,
    column: usize,
}

struct PendingInherit {
    derived_name: String,
    base_name: String,
    file_id: i64,
    access: Visibility,
    is_virtual: bool,
}

#[derive(Default)]
struct StreamInsertState {
    next_def_id: i64,
    metrics: IncrementalDbUpdate,
    qname_to_db_id: FxHashMap<String, i64>,
    name_to_db_id: FxHashMap<String, Option<i64>>,
    pending_refs: Vec<PendingReference>,
    pending_calls: Vec<PendingCall>,
    pending_inherits: Vec<PendingInherit>,
}

#[derive(Debug, Clone)]
pub struct IndexConfig {
    pub roots: Vec<PathBuf>,
    pub language: String,
    pub follow_symlinks: bool,
    pub reference_mode: String,
    pub include_reference_context: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IncrementalDbUpdate {
    pub definitions_inserted: usize,
    pub references_inserted: usize,
    pub calls_inserted: usize,
    pub inheritance_inserted: usize,
}

struct DeletedFileInfo {
    file_id: i64,
    old_def_ids: Vec<i64>,
    old_names: Vec<String>,
}

/// Create the xref schema in the database.
pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    create_tables(conn)?;
    create_indexes(conn)
}

fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            checksum TEXT,
            symbols_checksum TEXT,
            indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
        );

        CREATE TABLE IF NOT EXISTS definitions (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            file_id INTEGER REFERENCES files(id),
            line INTEGER NOT NULL,
            column INTEGER NOT NULL,
            end_line INTEGER NOT NULL DEFAULT 0,
            end_column INTEGER NOT NULL DEFAULT 0,
            parent_name TEXT,
            signature TEXT,
            visibility TEXT NOT NULL DEFAULT 'public',
            is_definition INTEGER NOT NULL DEFAULT 1,
            extra TEXT
        );

        CREATE TABLE IF NOT EXISTS refs (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT 'function',
            file_id INTEGER REFERENCES files(id),
            line INTEGER NOT NULL,
            column INTEGER NOT NULL,
            def_id INTEGER REFERENCES definitions(id),
            context TEXT
        );

        CREATE TABLE IF NOT EXISTS call_graph (
            id INTEGER PRIMARY KEY,
            caller_id INTEGER REFERENCES definitions(id),
            callee_id INTEGER REFERENCES definitions(id),
            file_id INTEGER REFERENCES files(id),
            line INTEGER NOT NULL,
            column INTEGER NOT NULL DEFAULT 0,
            caller_name TEXT,
            callee_name TEXT
        );

        CREATE TABLE IF NOT EXISTS inheritance (
            id INTEGER PRIMARY KEY,
            derived_id INTEGER REFERENCES definitions(id),
            base_id INTEGER REFERENCES definitions(id),
            file_id INTEGER REFERENCES files(id),
            access TEXT NOT NULL DEFAULT 'public',
            is_virtual INTEGER NOT NULL DEFAULT 0,
            derived_name TEXT,
            base_name TEXT
        );

        CREATE TABLE IF NOT EXISTS index_roots (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            follow_symlinks INTEGER NOT NULL DEFAULT 0,
            reference_mode TEXT NOT NULL DEFAULT 'none',
            include_reference_context INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        );

        ",
    )?;
    ensure_parent_name_column(conn)?;
    ensure_files_symbols_checksum_column(conn)?;
    ensure_inheritance_file_id_column(conn)
}

fn create_indexes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_definitions_name ON definitions(name);
        CREATE INDEX IF NOT EXISTS idx_definitions_qualified ON definitions(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_definitions_kind ON definitions(kind);
        CREATE INDEX IF NOT EXISTS idx_definitions_file ON definitions(file_id);
        CREATE INDEX IF NOT EXISTS idx_refs_name ON refs(name);
        CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);
        CREATE INDEX IF NOT EXISTS idx_refs_def ON refs(def_id);
        CREATE INDEX IF NOT EXISTS idx_call_graph_caller ON call_graph(caller_id);
        CREATE INDEX IF NOT EXISTS idx_call_graph_callee ON call_graph(callee_id);
        CREATE INDEX IF NOT EXISTS idx_call_graph_caller_name ON call_graph(caller_name);
        CREATE INDEX IF NOT EXISTS idx_call_graph_callee_name ON call_graph(callee_name);
        CREATE INDEX IF NOT EXISTS idx_inheritance_derived ON inheritance(derived_id);
        CREATE INDEX IF NOT EXISTS idx_inheritance_base ON inheritance(base_id);
        CREATE INDEX IF NOT EXISTS idx_inheritance_file ON inheritance(file_id);
        CREATE INDEX IF NOT EXISTS idx_inheritance_derived_name ON inheritance(derived_name);
        CREATE INDEX IF NOT EXISTS idx_inheritance_base_name ON inheritance(base_name);
        ",
    )
}

fn drop_indexes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_definitions_name;
        DROP INDEX IF EXISTS idx_definitions_qualified;
        DROP INDEX IF EXISTS idx_definitions_kind;
        DROP INDEX IF EXISTS idx_definitions_file;
        DROP INDEX IF EXISTS idx_refs_name;
        DROP INDEX IF EXISTS idx_refs_file;
        DROP INDEX IF EXISTS idx_refs_def;
        DROP INDEX IF EXISTS idx_call_graph_caller;
        DROP INDEX IF EXISTS idx_call_graph_callee;
        DROP INDEX IF EXISTS idx_call_graph_caller_name;
        DROP INDEX IF EXISTS idx_call_graph_callee_name;
        DROP INDEX IF EXISTS idx_inheritance_derived;
        DROP INDEX IF EXISTS idx_inheritance_base;
        DROP INDEX IF EXISTS idx_inheritance_file;
        DROP INDEX IF EXISTS idx_inheritance_derived_name;
        DROP INDEX IF EXISTS idx_inheritance_base_name;
        ",
    )
}

fn ensure_parent_name_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(definitions)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut has_parent_name = false;
    for column in columns {
        if column? == "parent_name" {
            has_parent_name = true;
            break;
        }
    }
    if !has_parent_name {
        conn.execute_batch("ALTER TABLE definitions ADD COLUMN parent_name TEXT;")?;
    }
    Ok(())
}

fn ensure_files_symbols_checksum_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(files)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut has_symbols_checksum = false;
    for column in columns {
        if column? == "symbols_checksum" {
            has_symbols_checksum = true;
            break;
        }
    }
    if !has_symbols_checksum {
        conn.execute_batch("ALTER TABLE files ADD COLUMN symbols_checksum TEXT;")?;
    }
    Ok(())
}

fn ensure_inheritance_file_id_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(inheritance)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut has_file_id = false;
    for column in columns {
        if column? == "file_id" {
            has_file_id = true;
            break;
        }
    }
    if !has_file_id {
        conn.execute_batch(
            "ALTER TABLE inheritance ADD COLUMN file_id INTEGER REFERENCES files(id);",
        )?;
    }
    Ok(())
}

fn resolve_db_definition_id(
    qname_to_db_id: &FxHashMap<&str, i64>,
    name_to_db_ids: &FxHashMap<&str, Vec<i64>>,
    name: &str,
) -> Option<i64> {
    qname_to_db_id.get(name).copied().or_else(|| {
        name_to_db_ids
            .get(name)
            .and_then(|ids| (ids.len() == 1).then_some(ids[0]))
    })
}

pub fn checksum_bytes(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}:{}", bytes.len())
}

pub fn checksum_path(path: &Path) -> std::io::Result<(String, u64)> {
    let bytes = std::fs::read(path)?;
    Ok((checksum_bytes(&bytes), bytes.len() as u64))
}

pub fn file_symbols_checksum(symbols: &FileSymbols) -> String {
    let mut hash = FNV_OFFSET;
    hash_file_symbols(&mut hash, symbols);
    format!("{hash:016x}")
}

fn index_symbol_checksums(index: &Index) -> FxHashMap<PathBuf, String> {
    let mut hashes: FxHashMap<PathBuf, u64> = FxHashMap::default();
    for file in index.files() {
        hashes.insert(file.path.clone(), FNV_OFFSET);
    }
    for def in index.definitions() {
        let hash = hashes
            .entry(def.location.file.clone())
            .or_insert(FNV_OFFSET);
        hash_str(hash, "def");
        hash_str(hash, &def.name);
        hash_str(hash, &def.qualified_name);
        hash_str(hash, def.kind.as_str());
        hash_location(hash, &def.location);
        hash_opt_str(hash, def.parent.as_deref());
        hash_opt_str(hash, def.signature.as_deref());
        hash_str(hash, def.visibility.as_str());
        hash_bool(hash, def.is_definition);
        hash_opt_str(hash, def.extra.as_deref());
    }
    for reference in index.references() {
        let hash = hashes
            .entry(reference.location.file.clone())
            .or_insert(FNV_OFFSET);
        hash_str(hash, "ref");
        hash_str(hash, &reference.name);
        hash_str(hash, reference.kind.as_str());
        hash_location(hash, &reference.location);
    }
    for call in index.calls() {
        let hash = hashes
            .entry(call.location.file.clone())
            .or_insert(FNV_OFFSET);
        hash_str(hash, "call");
        hash_str(hash, &call.caller_name);
        hash_str(hash, &call.callee_name);
        hash_location(hash, &call.location);
    }
    for inherit in index.inherits() {
        let hash = hashes
            .entry(inherit.location.file.clone())
            .or_insert(FNV_OFFSET);
        hash_str(hash, "inherit");
        hash_str(hash, &inherit.derived_name);
        hash_str(hash, &inherit.base_name);
        hash_location(hash, &inherit.location);
        hash_str(hash, inherit.access.as_str());
        hash_bool(hash, inherit.is_virtual);
    }
    hashes
        .into_iter()
        .map(|(path, hash)| (path, format!("{hash:016x}")))
        .collect()
}

fn hash_file_symbols(hash: &mut u64, symbols: &FileSymbols) {
    for def in &symbols.definitions {
        hash_str(hash, "def");
        hash_str(hash, &def.name);
        hash_str(hash, &def.qualified_name);
        hash_str(hash, def.kind.as_str());
        hash_location(hash, &def.location);
        hash_opt_str(hash, def.parent.as_deref());
        hash_opt_str(hash, def.signature.as_deref());
        hash_str(hash, def.visibility.as_str());
        hash_bool(hash, def.is_definition);
        hash_opt_str(hash, def.extra.as_deref());
    }
    for reference in &symbols.references {
        hash_str(hash, "ref");
        hash_str(hash, &reference.name);
        hash_str(hash, reference.kind.as_str());
        hash_location(hash, &reference.location);
    }
    for call in &symbols.calls {
        hash_str(hash, "call");
        hash_str(hash, &call.caller_name);
        hash_str(hash, &call.callee_name);
        hash_location(hash, &call.location);
    }
    for inherit in &symbols.inherits {
        hash_str(hash, "inherit");
        hash_str(hash, &inherit.derived_name);
        hash_str(hash, &inherit.base_name);
        hash_location(hash, &inherit.location);
        hash_str(hash, inherit.access.as_str());
        hash_bool(hash, inherit.is_virtual);
    }
}

fn hash_location(hash: &mut u64, location: &Location) {
    hash_str(hash, location.file.to_string_lossy().as_ref());
    hash_usize(hash, location.line);
    hash_usize(hash, location.column);
    hash_usize(hash, location.end_line);
    hash_usize(hash, location.end_column);
}

fn hash_opt_str(hash: &mut u64, value: Option<&str>) {
    if let Some(value) = value {
        hash_bool(hash, true);
        hash_str(hash, value);
    } else {
        hash_bool(hash, false);
    }
}

fn hash_bool(hash: &mut u64, value: bool) {
    hash_byte(hash, u8::from(value));
}

fn hash_usize(hash: &mut u64, value: usize) {
    for byte in value.to_le_bytes() {
        hash_byte(hash, byte);
    }
}

fn hash_str(hash: &mut u64, value: &str) {
    for byte in value.as_bytes() {
        hash_byte(hash, *byte);
    }
    hash_byte(hash, 0xff);
}

fn hash_byte(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(FNV_PRIME);
}

/// Persist an [Index] to a SQLite database at `path`.
/// Replaces any existing data in the database.
/// Returns the [Connection] so callers can run further queries.
pub fn save_to_db(index: &Index, path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=OFF;
        PRAGMA synchronous=OFF;
        PRAGMA foreign_keys=OFF;
        PRAGMA temp_store=MEMORY;
        PRAGMA locking_mode=EXCLUSIVE;
        PRAGMA cache_size=-200000;
        ",
    )?;
    create_tables(&conn)?;
    drop_indexes(&conn)?;

    let tx = conn.unchecked_transaction()?;

    // Clear existing data so repeated saves don't duplicate rows.
    tx.execute_batch(
        "DELETE FROM inheritance; DELETE FROM call_graph; DELETE FROM refs; DELETE FROM definitions; DELETE FROM files; DELETE FROM index_roots;",
    )?;

    // Insert files from the index's first-class file collection. New indexes
    // carry checksums from the parallel parse stage; fall back for older
    // programmatic Index::new callers.
    let symbol_checksums = if index
        .files()
        .iter()
        .any(|file| file.symbols_checksum.is_none())
    {
        index_symbol_checksums(index)
    } else {
        FxHashMap::default()
    };
    let mut file_ids: FxHashMap<PathBuf, i64> = FxHashMap::default();
    file_ids.reserve(index.files().len());
    {
        let mut insert_stmt = tx.prepare(
            "INSERT INTO files (id, path, language, checksum, symbols_checksum) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (i, f) in index.files().iter().enumerate() {
            let db_id = i as i64 + 1;
            let path_str = f.path.to_str().unwrap_or("");
            let checksum = f
                .checksum
                .clone()
                .or_else(|| checksum_path(&f.path).ok().map(|(checksum, _)| checksum));
            let symbols_checksum = f
                .symbols_checksum
                .as_ref()
                .or_else(|| symbol_checksums.get(&f.path));
            insert_stmt.execute(params![
                db_id,
                path_str,
                f.language.as_str(),
                checksum,
                symbols_checksum
            ])?;
            file_ids.insert(f.path.clone(), db_id);
        }
    }

    // Insert definitions; build maps for later resolution.
    let mut def_id_map: Vec<i64> = Vec::with_capacity(index.definitions().len());
    let mut def_file_ids: Vec<Option<i64>> = Vec::with_capacity(index.definitions().len());
    let mut qname_to_db_id: FxHashMap<&str, i64> = FxHashMap::default();
    let mut name_to_db_ids: FxHashMap<&str, Vec<i64>> = FxHashMap::default();
    qname_to_db_id.reserve(index.definitions().len());
    name_to_db_ids.reserve(index.definitions().len());
    {
        let mut stmt = tx.prepare(
            "INSERT INTO definitions (id, name, qualified_name, kind, file_id, line, column, end_line, end_column, parent_name, signature, visibility, is_definition, extra) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        )?;
        for (i, d) in index.definitions().iter().enumerate() {
            let db_id = i as i64 + 1;
            let file_id = file_ids.get(&d.location.file).copied();
            stmt.execute(params![
                db_id,
                d.name,
                d.qualified_name,
                d.kind.as_str(),
                file_id,
                d.location.line as i64,
                d.location.column as i64,
                d.location.end_line as i64,
                d.location.end_column as i64,
                d.parent.as_ref(),
                d.signature,
                d.visibility.as_str(),
                d.is_definition as i64,
                d.extra,
            ])?;
            def_id_map.push(db_id);
            def_file_ids.push(file_id);
            qname_to_db_id.insert(&d.qualified_name, db_id);
            name_to_db_ids.entry(&d.name).or_default().push(db_id);
        }
    }

    // Insert references, remapping def_id from in-memory DefId to DB row id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO refs (name, kind, file_id, line, column, def_id, context) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for r in index.references() {
            let file_id = file_ids.get(&r.location.file).copied();
            let db_def_id = r
                .def_id
                .and_then(|def_id| def_id_map.get(def_id.0))
                .copied();
            stmt.execute(params![
                r.name,
                r.kind.as_str(),
                file_id,
                r.location.line as i64,
                r.location.column as i64,
                db_def_id,
                r.context,
            ])?;
        }
    }

    // Insert call graph with resolved IDs.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO call_graph (caller_id, callee_id, caller_name, callee_name, file_id, line, column) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for call in index.calls() {
            let file_id = file_ids.get(&call.location.file).copied();
            let caller_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &call.caller_name);
            let callee_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &call.callee_name);
            stmt.execute(params![
                caller_id,
                callee_id,
                call.caller_name,
                call.callee_name,
                file_id,
                call.location.line as i64,
                call.location.column as i64,
            ])?;
        }
    }

    // Insert inheritance with resolved IDs.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO inheritance (derived_id, base_id, file_id, derived_name, base_name, access, is_virtual) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for inh in index.inherits() {
            let derived_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &inh.derived_name);
            let base_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &inh.base_name);
            let file_id = file_ids.get(&inh.location.file).copied().or_else(|| {
                derived_id
                    .and_then(|id| id.checked_sub(1))
                    .and_then(|pos| def_file_ids.get(pos as usize).copied().flatten())
            });
            stmt.execute(params![
                derived_id,
                base_id,
                file_id,
                inh.derived_name,
                inh.base_name,
                inh.access.as_str(),
                inh.is_virtual as i64,
            ])?;
        }
    }

    tx.commit()?;
    create_indexes(&conn)?;
    // Re-enable safer defaults for future queries.
    conn.execute_batch(
        "
        PRAGMA foreign_keys=ON;
        PRAGMA locking_mode=NORMAL;
        PRAGMA optimize;
        ",
    )?;
    Ok(conn)
}

pub fn save_streamed_to_db(
    path: &Path,
    receiver: Receiver<Result<StreamedFileSymbols, String>>,
) -> Result<(Connection, IncrementalDbUpdate, StreamedDbWriteMetrics), String> {
    let writer_start = Instant::now();
    let conn = Connection::open(path)
        .map_err(|e| format!("failed to open database '{}': {e}", path.display()))?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=OFF;
        PRAGMA synchronous=OFF;
        PRAGMA foreign_keys=OFF;
        PRAGMA temp_store=MEMORY;
        PRAGMA locking_mode=EXCLUSIVE;
        PRAGMA cache_size=-200000;
        ",
    )
    .map_err(|e| format!("failed to configure database '{}': {e}", path.display()))?;
    create_tables(&conn).map_err(|e| format!("failed to prepare database tables: {e}"))?;
    drop_indexes(&conn).map_err(|e| format!("failed to drop database indexes: {e}"))?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("failed to start database transaction: {e}"))?;
    tx.execute_batch(
        "DELETE FROM inheritance; DELETE FROM call_graph; DELETE FROM refs; DELETE FROM definitions; DELETE FROM files; DELETE FROM index_roots;",
    )
    .map_err(|e| format!("failed to clear database: {e}"))?;
    let setup_ms = writer_start.elapsed().as_millis();

    let mut state = StreamInsertState {
        next_def_id: 1,
        ..StreamInsertState::default()
    };

    let streamed_start = Instant::now();
    {
        let mut insert_file_stmt = tx
            .prepare(
                "
                INSERT INTO files (id, path, language, checksum, symbols_checksum, indexed_at)
                VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
                ",
            )
            .map_err(|e| format!("failed to prepare file insert: {e}"))?;
        let mut insert_def_stmt = tx
            .prepare(
                "
                INSERT INTO definitions (
                    id, name, qualified_name, kind, file_id, line, column,
                    end_line, end_column, parent_name, signature, visibility,
                    is_definition, extra
                )
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                ",
            )
            .map_err(|e| format!("failed to prepare definition insert: {e}"))?;

        let mut next_index = 0usize;
        let mut pending: BTreeMap<usize, StreamedFileSymbols> = BTreeMap::new();

        while let Ok(message) = receiver.recv() {
            let file = message?;
            pending.insert(file.index, file);
            while let Some(file) = pending.remove(&next_index) {
                insert_streamed_file_symbols(
                    &mut insert_file_stmt,
                    &mut insert_def_stmt,
                    &mut state,
                    file,
                )
                .map_err(|e| format!("failed to insert parsed file {next_index}: {e}"))?;
                next_index += 1;
            }
        }

        if !pending.is_empty() {
            return Err(format!(
                "parser stream ended before file index {next_index}; {} parsed files are pending",
                pending.len()
            ));
        }
    }
    let streamed_files_and_definitions_ms = streamed_start.elapsed().as_millis();

    let pending_edges_start = Instant::now();
    insert_streamed_pending_edges(&tx, &mut state)
        .map_err(|e| format!("failed to insert resolved references: {e}"))?;
    let pending_edges_ms = pending_edges_start.elapsed().as_millis();

    let commit_start = Instant::now();
    tx.commit()
        .map_err(|e| format!("failed to commit database transaction: {e}"))?;
    let commit_ms = commit_start.elapsed().as_millis();

    let create_indexes_start = Instant::now();
    create_indexes(&conn).map_err(|e| format!("failed to create database indexes: {e}"))?;
    let create_indexes_ms = create_indexes_start.elapsed().as_millis();

    let finalize_start = Instant::now();
    conn.execute_batch(
        "
        PRAGMA foreign_keys=ON;
        PRAGMA locking_mode=NORMAL;
        PRAGMA optimize;
        ",
    )
    .map_err(|e| format!("failed to finalize database: {e}"))?;
    let finalize_ms = finalize_start.elapsed().as_millis();

    Ok((
        conn,
        state.metrics,
        StreamedDbWriteMetrics {
            setup_ms,
            streamed_files_and_definitions_ms,
            pending_edges_ms,
            commit_ms,
            create_indexes_ms,
            finalize_ms,
            total_ms: writer_start.elapsed().as_millis(),
        },
    ))
}

pub fn apply_incremental_update(
    conn: &mut Connection,
    updates: &[IncrementalFileUpdate<'_>],
    checksum_updates: &[IncrementalChecksumUpdate<'_>],
    removed_paths: &[PathBuf],
) -> rusqlite::Result<IncrementalDbUpdate> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys=OFF;
        PRAGMA temp_store=MEMORY;
        PRAGMA cache_size=-200000;
        ",
    )?;
    create_schema(conn)?;

    let tx = conn.unchecked_transaction()?;
    let mut metrics = IncrementalDbUpdate::default();
    let mut affected_file_ids = Vec::new();
    let mut old_def_ids = Vec::new();
    let mut lookup_names: FxHashSet<String> = FxHashSet::default();
    let mut propagate_names: FxHashSet<String> = FxHashSet::default();

    for update in checksum_updates {
        update_file_checksums(&tx, update.path, update.checksum, update.symbols_checksum)?;
    }

    for path in removed_paths {
        if let Some(deleted) = delete_file_rows(&tx, path, true)? {
            old_def_ids.extend(deleted.old_def_ids);
            lookup_names.extend(deleted.old_names.iter().cloned());
            propagate_names.extend(deleted.old_names);
        }
    }

    for update in updates {
        let deleted = delete_file_rows(&tx, &update.symbols.file, false)?;
        if let Some(deleted) = &deleted {
            affected_file_ids.push(deleted.file_id);
            old_def_ids.extend(deleted.old_def_ids.iter().copied());
            lookup_names.extend(deleted.old_names.iter().cloned());
        }
        let file_id = upsert_file_row(
            &tx,
            &update.symbols.file,
            &update.symbols.language,
            update.checksum,
            update.symbols_checksum,
        )?;
        affected_file_ids.push(file_id);
        collect_lookup_names(update.symbols, &mut lookup_names);
        let new_definition_names = definition_names(update.symbols);
        lookup_names.extend(new_definition_names.iter().cloned());
        if let Some(deleted) = deleted {
            let old_names: FxHashSet<String> = deleted.old_names.into_iter().collect();
            propagate_names.extend(old_names.difference(&new_definition_names).cloned());
            propagate_names.extend(new_definition_names.difference(&old_names).cloned());
        } else {
            propagate_names.extend(new_definition_names);
        }
        insert_file_symbols(&tx, file_id, update.symbols, &mut metrics)?;
    }

    relink_symbol_ids(
        &tx,
        &affected_file_ids,
        &old_def_ids,
        &lookup_names,
        &propagate_names,
    )?;
    tx.commit()?;

    conn.execute_batch(
        "
        PRAGMA foreign_keys=ON;
        PRAGMA optimize;
        ",
    )?;
    Ok(metrics)
}

pub fn save_index_config(
    conn: &Connection,
    roots: &[PathBuf],
    language: &str,
    follow_symlinks: bool,
    reference_mode: &str,
    include_reference_context: bool,
) -> rusqlite::Result<()> {
    create_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM index_roots", [])?;
    {
        let mut stmt = tx.prepare(
            "
            INSERT INTO index_roots (
                path,
                language,
                follow_symlinks,
                reference_mode,
                include_reference_context,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
            ",
        )?;
        for root in roots {
            stmt.execute(params![
                root.to_string_lossy().as_ref(),
                language,
                follow_symlinks as i64,
                reference_mode,
                include_reference_context as i64,
            ])?;
        }
    }
    tx.commit()
}

pub fn load_index_config(conn: &Connection) -> rusqlite::Result<Option<IndexConfig>> {
    create_schema(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT path, language, follow_symlinks, reference_mode, include_reference_context
        FROM index_roots
        ORDER BY id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? != 0,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)? != 0,
        ))
    })?;

    let mut roots = Vec::new();
    let mut language = None;
    let mut follow_symlinks = false;
    let mut reference_mode = None;
    let mut include_reference_context = false;
    for row in rows {
        let (root, row_language, row_follow_symlinks, row_reference_mode, row_context) = row?;
        roots.push(root);
        language.get_or_insert(row_language);
        reference_mode.get_or_insert(row_reference_mode);
        follow_symlinks = row_follow_symlinks;
        include_reference_context = row_context;
    }

    if roots.is_empty() {
        return Ok(None);
    }
    Ok(Some(IndexConfig {
        roots,
        language: language.unwrap_or_else(|| "cpp".to_string()),
        follow_symlinks,
        reference_mode: reference_mode.unwrap_or_else(|| "none".to_string()),
        include_reference_context,
    }))
}

pub fn infer_index_config_from_files(conn: &Connection) -> rusqlite::Result<Option<IndexConfig>> {
    create_schema(conn)?;
    let mut stmt = conn.prepare("SELECT path, language FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;

    let mut paths = Vec::new();
    let mut languages = BTreeMap::<String, usize>::new();
    for row in rows {
        let (path, language) = row?;
        paths.push(path);
        *languages.entry(language).or_default() += 1;
    }
    if paths.is_empty() {
        return Ok(None);
    }

    let Some(root) = infer_common_index_root(&paths) else {
        return Ok(None);
    };
    if is_filesystem_root(&root) || !root.exists() {
        return Ok(None);
    }

    let language = languages
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(language, _)| language)
        .unwrap_or_else(|| "cpp".to_string());

    Ok(Some(IndexConfig {
        roots: vec![root],
        language,
        follow_symlinks: false,
        reference_mode: "none".to_string(),
        include_reference_context: false,
    }))
}

fn infer_common_index_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut parents = paths.iter().map(|path| file_parent(path));
    let mut common = parents.next()?;

    for parent in parents {
        while !parent.starts_with(&common) {
            if !common.pop() || common.as_os_str().is_empty() {
                return Some(PathBuf::from("."));
            }
        }
    }

    if common.as_os_str().is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(common)
    }
}

fn file_parent(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        parent.to_path_buf()
    }
}

fn is_filesystem_root(path: &Path) -> bool {
    path.has_root() && path.components().count() == 1
}

fn delete_file_rows(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    delete_file: bool,
) -> rusqlite::Result<Option<DeletedFileInfo>> {
    let path_str = path.to_string_lossy();
    let file_id = tx.query_row(
        "SELECT id FROM files WHERE path = ?1",
        params![path_str.as_ref()],
        |row| row.get::<_, i64>(0),
    );
    let Ok(file_id) = file_id else {
        return Ok(None);
    };

    let (old_def_ids, old_names) = {
        let mut stmt =
            tx.prepare("SELECT id, name, qualified_name FROM definitions WHERE file_id = ?1")?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut ids = Vec::new();
        let mut names = Vec::new();
        for row in rows {
            let (id, name, qualified_name) = row?;
            ids.push(id);
            names.push(name);
            names.push(qualified_name);
        }
        (ids, names)
    };

    tx.execute(
        "DELETE FROM inheritance WHERE file_id = ?1",
        params![file_id],
    )?;
    {
        let mut stmt =
            tx.prepare("DELETE FROM inheritance WHERE file_id IS NULL AND derived_id = ?1")?;
        for def_id in &old_def_ids {
            stmt.execute(params![def_id])?;
        }
    }
    {
        let mut stmt =
            tx.prepare("DELETE FROM inheritance WHERE file_id IS NULL AND derived_name = ?1")?;
        for name in &old_names {
            stmt.execute(params![name])?;
        }
    }
    tx.execute(
        "DELETE FROM call_graph WHERE file_id = ?1",
        params![file_id],
    )?;
    tx.execute("DELETE FROM refs WHERE file_id = ?1", params![file_id])?;
    tx.execute(
        "DELETE FROM definitions WHERE file_id = ?1",
        params![file_id],
    )?;
    if delete_file {
        tx.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
    }
    Ok(Some(DeletedFileInfo {
        file_id,
        old_def_ids,
        old_names,
    }))
}

fn upsert_file_row(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    language: &str,
    checksum: &str,
    symbols_checksum: &str,
) -> rusqlite::Result<i64> {
    let path_str = path.to_string_lossy();
    tx.execute(
        "
        INSERT INTO files (path, language, checksum, symbols_checksum, indexed_at)
        VALUES (?1, ?2, ?3, ?4, unixepoch())
        ON CONFLICT(path) DO UPDATE SET
            language = excluded.language,
            checksum = excluded.checksum,
            symbols_checksum = excluded.symbols_checksum,
            indexed_at = unixepoch()
        ",
        params![path_str.as_ref(), language, checksum, symbols_checksum],
    )?;
    tx.query_row(
        "SELECT id FROM files WHERE path = ?1",
        params![path_str.as_ref()],
        |row| row.get(0),
    )
}

fn update_file_checksums(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    checksum: &str,
    symbols_checksum: &str,
) -> rusqlite::Result<()> {
    let path_str = path.to_string_lossy();
    tx.execute(
        "
        UPDATE files
        SET checksum = ?2,
            symbols_checksum = ?3,
            indexed_at = unixepoch()
        WHERE path = ?1
        ",
        params![path_str.as_ref(), checksum, symbols_checksum],
    )?;
    Ok(())
}

fn insert_file_symbols(
    tx: &rusqlite::Transaction<'_>,
    file_id: i64,
    symbols: &FileSymbols,
    metrics: &mut IncrementalDbUpdate,
) -> rusqlite::Result<()> {
    {
        let mut stmt = tx.prepare(
            "INSERT INTO definitions (name, qualified_name, kind, file_id, line, column, end_line, end_column, parent_name, signature, visibility, is_definition, extra) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        for d in &symbols.definitions {
            stmt.execute(params![
                d.name,
                d.qualified_name,
                d.kind.as_str(),
                file_id,
                d.location.line as i64,
                d.location.column as i64,
                d.location.end_line as i64,
                d.location.end_column as i64,
                d.parent.as_ref(),
                d.signature,
                d.visibility.as_str(),
                d.is_definition as i64,
                d.extra,
            ])?;
        }
    }
    metrics.definitions_inserted += symbols.definitions.len();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO refs (name, kind, file_id, line, column, def_id, context) VALUES (?1,?2,?3,?4,?5,NULL,?6)",
        )?;
        for r in &symbols.references {
            stmt.execute(params![
                r.name,
                r.kind.as_str(),
                file_id,
                r.location.line as i64,
                r.location.column as i64,
                r.context,
            ])?;
        }
    }
    metrics.references_inserted += symbols.references.len();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO call_graph (caller_id, callee_id, caller_name, callee_name, file_id, line, column) VALUES (NULL,NULL,?1,?2,?3,?4,?5)",
        )?;
        for call in &symbols.calls {
            stmt.execute(params![
                call.caller_name,
                call.callee_name,
                file_id,
                call.location.line as i64,
                call.location.column as i64,
            ])?;
        }
    }
    metrics.calls_inserted += symbols.calls.len();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO inheritance (derived_id, base_id, file_id, derived_name, base_name, access, is_virtual) VALUES (NULL,NULL,?1,?2,?3,?4,?5)",
        )?;
        for inh in &symbols.inherits {
            stmt.execute(params![
                file_id,
                inh.derived_name,
                inh.base_name,
                inh.access.as_str(),
                inh.is_virtual as i64,
            ])?;
        }
    }
    metrics.inheritance_inserted += symbols.inherits.len();

    Ok(())
}

fn insert_streamed_file_symbols(
    insert_file_stmt: &mut rusqlite::Statement<'_>,
    insert_def_stmt: &mut rusqlite::Statement<'_>,
    state: &mut StreamInsertState,
    file: StreamedFileSymbols,
) -> rusqlite::Result<()> {
    let file_id = file.index as i64 + 1;
    let path_str = file.symbols.file.to_string_lossy();
    insert_file_stmt.execute(params![
        file_id,
        path_str.as_ref(),
        file.symbols.language.as_str(),
        file.checksum.as_str(),
        file.symbols_checksum.as_str(),
    ])?;

    let FileSymbols {
        file: _,
        language: _,
        definitions,
        references,
        calls,
        inherits,
    } = file.symbols;

    for d in definitions {
        let def_id = state.next_def_id;
        state.next_def_id += 1;
        let name_key = d.name.clone();
        let qname_key = d.qualified_name.clone();
        insert_def_stmt.execute(params![
            def_id,
            d.name,
            d.qualified_name,
            d.kind.as_str(),
            file_id,
            d.location.line as i64,
            d.location.column as i64,
            d.location.end_line as i64,
            d.location.end_column as i64,
            d.parent.as_ref(),
            d.signature,
            d.visibility.as_str(),
            d.is_definition as i64,
            d.extra,
        ])?;
        state.qname_to_db_id.insert(qname_key, def_id);
        state
            .name_to_db_id
            .entry(name_key)
            .and_modify(|existing| *existing = None)
            .or_insert(Some(def_id));
        state.metrics.definitions_inserted += 1;
    }

    state.pending_refs.reserve(references.len());
    for r in references {
        state.pending_refs.push(PendingReference {
            name: r.name,
            kind: r.kind,
            file_id,
            line: r.location.line,
            column: r.location.column,
            context: r.context,
        });
    }

    state.pending_calls.reserve(calls.len());
    for call in calls {
        state.pending_calls.push(PendingCall {
            caller_name: call.caller_name,
            callee_name: call.callee_name,
            file_id,
            line: call.location.line,
            column: call.location.column,
        });
    }

    state.pending_inherits.reserve(inherits.len());
    for inherit in inherits {
        state.pending_inherits.push(PendingInherit {
            derived_name: inherit.derived_name,
            base_name: inherit.base_name,
            file_id,
            access: inherit.access,
            is_virtual: inherit.is_virtual,
        });
    }

    Ok(())
}

fn insert_streamed_pending_edges(
    tx: &rusqlite::Transaction<'_>,
    state: &mut StreamInsertState,
) -> rusqlite::Result<()> {
    {
        let mut stmt = tx.prepare(
            "INSERT INTO refs (name, kind, file_id, line, column, def_id, context) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for reference in &state.pending_refs {
            let def_id = resolve_streamed_db_definition_id(state, &reference.name);
            stmt.execute(params![
                reference.name,
                reference.kind.as_str(),
                reference.file_id,
                reference.line as i64,
                reference.column as i64,
                def_id,
                reference.context,
            ])?;
        }
    }
    state.metrics.references_inserted += state.pending_refs.len();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO call_graph (caller_id, callee_id, caller_name, callee_name, file_id, line, column) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for call in &state.pending_calls {
            let caller_id = resolve_streamed_db_definition_id(state, &call.caller_name);
            let callee_id = resolve_streamed_db_definition_id(state, &call.callee_name);
            stmt.execute(params![
                caller_id,
                callee_id,
                call.caller_name,
                call.callee_name,
                call.file_id,
                call.line as i64,
                call.column as i64,
            ])?;
        }
    }
    state.metrics.calls_inserted += state.pending_calls.len();

    {
        let mut stmt = tx.prepare(
            "INSERT INTO inheritance (derived_id, base_id, file_id, derived_name, base_name, access, is_virtual) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for inherit in &state.pending_inherits {
            let derived_id = resolve_streamed_db_definition_id(state, &inherit.derived_name);
            let base_id = resolve_streamed_db_definition_id(state, &inherit.base_name);
            stmt.execute(params![
                derived_id,
                base_id,
                inherit.file_id,
                inherit.derived_name,
                inherit.base_name,
                inherit.access.as_str(),
                inherit.is_virtual as i64,
            ])?;
        }
    }
    state.metrics.inheritance_inserted += state.pending_inherits.len();

    Ok(())
}

fn resolve_streamed_db_definition_id(state: &StreamInsertState, name: &str) -> Option<i64> {
    state
        .qname_to_db_id
        .get(name)
        .copied()
        .or_else(|| state.name_to_db_id.get(name).and_then(|id| *id))
}

fn definition_names(symbols: &FileSymbols) -> FxHashSet<String> {
    let mut names = FxHashSet::default();
    for def in &symbols.definitions {
        names.insert(def.name.clone());
        names.insert(def.qualified_name.clone());
    }
    names
}

fn collect_lookup_names(symbols: &FileSymbols, names: &mut FxHashSet<String>) {
    for reference in &symbols.references {
        names.insert(reference.name.clone());
    }
    for call in &symbols.calls {
        names.insert(call.caller_name.clone());
        names.insert(call.callee_name.clone());
    }
    for inherit in &symbols.inherits {
        names.insert(inherit.derived_name.clone());
        names.insert(inherit.base_name.clone());
    }
}

fn relink_symbol_ids(
    tx: &rusqlite::Transaction<'_>,
    affected_file_ids: &[i64],
    old_def_ids: &[i64],
    lookup_names: &FxHashSet<String>,
    propagate_names: &FxHashSet<String>,
) -> rusqlite::Result<()> {
    if affected_file_ids.is_empty()
        && old_def_ids.is_empty()
        && lookup_names.is_empty()
        && propagate_names.is_empty()
    {
        return Ok(());
    }

    tx.execute_batch(
        "
        DROP TABLE IF EXISTS temp.xref_affected_files;
        DROP TABLE IF EXISTS temp.xref_old_def_ids;
        DROP TABLE IF EXISTS temp.xref_lookup_names;
        DROP TABLE IF EXISTS temp.xref_propagate_names;
        DROP TABLE IF EXISTS temp.xref_qname_map;
        DROP TABLE IF EXISTS temp.xref_unique_name_map;
        DROP TABLE IF EXISTS temp.xref_ref_rows;
        DROP TABLE IF EXISTS temp.xref_call_rows;
        DROP TABLE IF EXISTS temp.xref_inherit_rows;

        CREATE TEMP TABLE xref_affected_files (id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE xref_old_def_ids (id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE xref_lookup_names (name TEXT PRIMARY KEY);
        CREATE TEMP TABLE xref_propagate_names (name TEXT PRIMARY KEY);
        CREATE TEMP TABLE xref_ref_rows (id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE xref_call_rows (id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE xref_inherit_rows (id INTEGER PRIMARY KEY);
        ",
    )?;

    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO xref_affected_files (id) VALUES (?1)")?;
        for file_id in affected_file_ids {
            stmt.execute(params![file_id])?;
        }
    }
    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO xref_old_def_ids (id) VALUES (?1)")?;
        for def_id in old_def_ids {
            stmt.execute(params![def_id])?;
        }
    }
    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO xref_lookup_names (name) VALUES (?1)")?;
        for name in lookup_names {
            stmt.execute(params![name])?;
        }
    }
    {
        let mut stmt =
            tx.prepare("INSERT OR IGNORE INTO xref_propagate_names (name) VALUES (?1)")?;
        for name in propagate_names {
            stmt.execute(params![name])?;
        }
    }

    tx.execute_batch(
        "
        CREATE TEMP TABLE xref_qname_map (
            name TEXT PRIMARY KEY,
            id INTEGER NOT NULL
        );
        INSERT INTO xref_qname_map
        SELECT d.qualified_name, MAX(d.id)
        FROM definitions d
        JOIN xref_lookup_names a ON a.name = d.qualified_name
        GROUP BY d.qualified_name;

        CREATE TEMP TABLE xref_unique_name_map (
            name TEXT PRIMARY KEY,
            id INTEGER NOT NULL
        );
        INSERT INTO xref_unique_name_map
        SELECT d.name, MIN(d.id)
        FROM definitions d
        JOIN xref_lookup_names a ON a.name = d.name
        GROUP BY d.name
        HAVING COUNT(*) = 1;

        INSERT OR IGNORE INTO xref_ref_rows
        SELECT id FROM refs WHERE file_id IN (SELECT id FROM xref_affected_files);
        INSERT OR IGNORE INTO xref_ref_rows
        SELECT id FROM refs WHERE def_id IN (SELECT id FROM xref_old_def_ids);
        INSERT OR IGNORE INTO xref_ref_rows
        SELECT id FROM refs WHERE name IN (SELECT name FROM xref_propagate_names);

        INSERT OR IGNORE INTO xref_call_rows
        SELECT id FROM call_graph WHERE file_id IN (SELECT id FROM xref_affected_files);
        INSERT OR IGNORE INTO xref_call_rows
        SELECT id FROM call_graph WHERE caller_id IN (SELECT id FROM xref_old_def_ids);
        INSERT OR IGNORE INTO xref_call_rows
        SELECT id FROM call_graph WHERE callee_id IN (SELECT id FROM xref_old_def_ids);
        INSERT OR IGNORE INTO xref_call_rows
        SELECT id FROM call_graph WHERE caller_name IN (SELECT name FROM xref_propagate_names);
        INSERT OR IGNORE INTO xref_call_rows
        SELECT id FROM call_graph WHERE callee_name IN (SELECT name FROM xref_propagate_names);

        INSERT OR IGNORE INTO xref_inherit_rows
        SELECT id FROM inheritance WHERE file_id IN (SELECT id FROM xref_affected_files);
        INSERT OR IGNORE INTO xref_inherit_rows
        SELECT id FROM inheritance WHERE derived_id IN (SELECT id FROM xref_old_def_ids);
        INSERT OR IGNORE INTO xref_inherit_rows
        SELECT id FROM inheritance WHERE base_id IN (SELECT id FROM xref_old_def_ids);
        INSERT OR IGNORE INTO xref_inherit_rows
        SELECT id FROM inheritance WHERE derived_name IN (SELECT name FROM xref_propagate_names);
        INSERT OR IGNORE INTO xref_inherit_rows
        SELECT id FROM inheritance WHERE base_name IN (SELECT name FROM xref_propagate_names);

        UPDATE refs
        SET def_id = COALESCE(
            (SELECT id FROM xref_qname_map WHERE name = refs.name),
            (SELECT id FROM xref_unique_name_map WHERE name = refs.name)
        )
        WHERE id IN (SELECT id FROM xref_ref_rows);

        UPDATE call_graph
        SET
            caller_id = COALESCE(
                (SELECT id FROM xref_qname_map WHERE name = call_graph.caller_name),
                (SELECT id FROM xref_unique_name_map WHERE name = call_graph.caller_name)
            ),
            callee_id = COALESCE(
                (SELECT id FROM xref_qname_map WHERE name = call_graph.callee_name),
                (SELECT id FROM xref_unique_name_map WHERE name = call_graph.callee_name)
            )
        WHERE id IN (SELECT id FROM xref_call_rows);

        UPDATE inheritance
        SET
            derived_id = COALESCE(
                (SELECT id FROM xref_qname_map WHERE name = inheritance.derived_name),
                (SELECT id FROM xref_unique_name_map WHERE name = inheritance.derived_name)
            ),
            base_id = COALESCE(
                (SELECT id FROM xref_qname_map WHERE name = inheritance.base_name),
                (SELECT id FROM xref_unique_name_map WHERE name = inheritance.base_name)
            )
        WHERE id IN (SELECT id FROM xref_inherit_rows);

        DROP TABLE IF EXISTS temp.xref_affected_files;
        DROP TABLE IF EXISTS temp.xref_old_def_ids;
        DROP TABLE IF EXISTS temp.xref_lookup_names;
        DROP TABLE IF EXISTS temp.xref_propagate_names;
        DROP TABLE IF EXISTS temp.xref_qname_map;
        DROP TABLE IF EXISTS temp.xref_unique_name_map;
        DROP TABLE IF EXISTS temp.xref_ref_rows;
        DROP TABLE IF EXISTS temp.xref_call_rows;
        DROP TABLE IF EXISTS temp.xref_inherit_rows;
        ",
    )
}

/// Load an [Index] from a SQLite database at `path`.
/// Returns both the [Index] and the [Connection] for further queries.
pub fn open_from_db(path: &Path) -> rusqlite::Result<(Index, Connection)> {
    let conn = Connection::open(path)?;
    create_schema(&conn)?;
    create_indexes(&conn)?;

    // Preload all files; build both the lookup map and the IndexedFile collection.
    let file_map: std::collections::HashMap<i64, PathBuf> = {
        let mut stmt = conn.prepare("SELECT id, path, language FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                std::path::PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let (id, path, _lang) = r?;
            map.insert(id, path);
        }
        map
    };

    let index_files: Vec<IndexedFile> = {
        let mut stmt =
            conn.prepare("SELECT path, language, checksum, symbols_checksum FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok(IndexedFile {
                path: std::path::PathBuf::from(row.get::<_, String>(0)?),
                language: row.get(1)?,
                checksum: row.get(2)?,
                symbols_checksum: row.get(3)?,
            })
        })?;
        let mut files = Vec::new();
        for f in rows {
            files.push(f?);
        }
        files
    };

    let resolve_file = |file_id: Option<i64>| -> PathBuf {
        file_id
            .and_then(|fid| file_map.get(&fid).cloned())
            .unwrap_or_default()
    };

    let mut definitions: Vec<Definition> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, name, qualified_name, kind, line, column, end_line, end_column, parent_name, signature, visibility, is_definition, extra, file_id FROM definitions",
        )?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(13)?;
            Ok(Definition {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                qualified_name: row.get(2)?,
                kind: SymbolKind::from_str(&row.get::<_, String>(3)?)
                    .unwrap_or(SymbolKind::Function),
                location: Location {
                    file: resolve_file(file_id),
                    line: row.get::<_, i64>(4)? as usize,
                    column: row.get::<_, i64>(5)? as usize,
                    end_line: row.get::<_, i64>(6)? as usize,
                    end_column: row.get::<_, i64>(7)? as usize,
                },
                parent: row.get(8)?,
                signature: row.get(9)?,
                visibility: Visibility::from_str(&row.get::<_, String>(10)?)
                    .unwrap_or(Visibility::Public),
                is_definition: row.get::<_, i64>(11)? != 0,
                extra: row.get(12)?,
            })
        })?;
        for d in rows {
            definitions.push(d.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
        }
    }

    // Build a map from DB definition ID → position in the definitions vector.
    let db_id_to_pos: std::collections::HashMap<i64, usize> = definitions
        .iter()
        .enumerate()
        .filter_map(|(pos, d)| d.id.map(|db_id| (db_id, pos)))
        .collect();

    let mut references: Vec<Reference> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, name, kind, line, column, def_id, context, file_id FROM refs")?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(7)?;
            let db_def_id: Option<i64> = row.get(5)?;
            Ok(Reference {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                kind: SymbolKind::from_str(&row.get::<_, String>(2)?)
                    .unwrap_or(SymbolKind::Function),
                location: Location {
                    file: resolve_file(file_id),
                    line: row.get::<_, i64>(3)? as usize,
                    column: row.get::<_, i64>(4)? as usize,
                    end_line: 0,
                    end_column: 0,
                },
                def_id: db_def_id.and_then(|db_id| db_id_to_pos.get(&db_id).map(|&pos| DefId(pos))),
                context: row.get(6)?,
            })
        })?;
        for r in rows {
            references.push(r.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
        }
    }

    let mut calls: Vec<CallEdge> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT caller_name, callee_name, line, column, file_id FROM call_graph")?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(4)?;
            Ok(CallEdge {
                caller_name: row.get(0)?,
                callee_name: row.get(1)?,
                location: Location {
                    file: resolve_file(file_id),
                    line: row.get::<_, i64>(2)? as usize,
                    column: row.get::<_, i64>(3)? as usize,
                    end_line: 0,
                    end_column: 0,
                },
            })
        })?;
        for c in rows {
            calls.push(c.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
        }
    }

    let mut inherits: Vec<InheritEdge> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT derived_name, base_name, access, is_virtual, file_id FROM inheritance",
        )?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(4)?;
            Ok(InheritEdge {
                derived_name: row.get(0)?,
                base_name: row.get(1)?,
                location: Location {
                    file: resolve_file(file_id),
                    line: 0,
                    column: 0,
                    end_line: 0,
                    end_column: 0,
                },
                access: Visibility::from_str(&row.get::<_, String>(2)?)
                    .unwrap_or(Visibility::Public),
                is_virtual: row.get::<_, i64>(3)? != 0,
            })
        })?;
        for inh in rows {
            inherits.push(inh.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
        }
    }

    Ok((
        Index::new(index_files, definitions, references, calls, inherits),
        conn,
    ))
}
