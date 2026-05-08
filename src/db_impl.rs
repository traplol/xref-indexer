use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use rustc_hash::FxHashMap;

use crate::index::Index;
use crate::types::*;

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
            access TEXT NOT NULL DEFAULT 'public',
            is_virtual INTEGER NOT NULL DEFAULT 0,
            derived_name TEXT,
            base_name TEXT
        );

        ",
    )?;
    ensure_parent_name_column(conn)
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
        "DELETE FROM inheritance; DELETE FROM call_graph; DELETE FROM refs; DELETE FROM definitions; DELETE FROM files;",
    )?;

    // Insert files from the index's first-class file collection.
    let mut file_ids: FxHashMap<PathBuf, i64> = FxHashMap::default();
    file_ids.reserve(index.files().len());
    {
        let mut insert_stmt =
            tx.prepare("INSERT INTO files (id, path, language) VALUES (?1, ?2, ?3)")?;
        for (i, f) in index.files().iter().enumerate() {
            let db_id = i as i64 + 1;
            let path_str = f.path.to_str().unwrap_or("");
            insert_stmt.execute(params![db_id, path_str, f.language.as_str()])?;
            file_ids.insert(f.path.clone(), db_id);
        }
    }

    // Insert definitions; build maps for later resolution.
    let mut def_id_map: Vec<i64> = Vec::with_capacity(index.definitions().len());
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
            "INSERT INTO inheritance (derived_id, base_id, derived_name, base_name, access, is_virtual) VALUES (?1,?2,?3,?4,?5,?6)",
        )?;
        for inh in index.inherits() {
            let derived_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &inh.derived_name);
            let base_id =
                resolve_db_definition_id(&qname_to_db_id, &name_to_db_ids, &inh.base_name);
            stmt.execute(params![
                derived_id,
                base_id,
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
        let mut stmt = conn.prepare("SELECT path, language FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok(IndexedFile {
                path: std::path::PathBuf::from(row.get::<_, String>(0)?),
                language: row.get(1)?,
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
        let mut stmt =
            conn.prepare("SELECT derived_name, base_name, access, is_virtual FROM inheritance")?;
        let rows = stmt.query_map([], |row| {
            Ok(InheritEdge {
                derived_name: row.get(0)?,
                base_name: row.get(1)?,
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
