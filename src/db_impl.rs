use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::index::Index;
use crate::types::*;

/// Create the xref schema in the database.
pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            checksum TEXT,
            indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
        );

        CREATE TABLE IF NOT EXISTS definitions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            file_id INTEGER REFERENCES files(id),
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

        CREATE TABLE IF NOT EXISTS refs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT 'function',
            file_id INTEGER REFERENCES files(id),
            line INTEGER NOT NULL,
            column INTEGER NOT NULL,
            def_id INTEGER REFERENCES definitions(id),
            context TEXT
        );

        CREATE TABLE IF NOT EXISTS call_graph (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            caller_id INTEGER REFERENCES definitions(id),
            callee_id INTEGER REFERENCES definitions(id),
            file_id INTEGER REFERENCES files(id),
            line INTEGER NOT NULL,
            column INTEGER NOT NULL DEFAULT 0,
            caller_name TEXT,
            callee_name TEXT
        );

        CREATE TABLE IF NOT EXISTS inheritance (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            derived_id INTEGER REFERENCES definitions(id),
            base_id INTEGER REFERENCES definitions(id),
            access TEXT NOT NULL DEFAULT 'public',
            is_virtual INTEGER NOT NULL DEFAULT 0,
            derived_name TEXT,
            base_name TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_definitions_name ON definitions(name);
        CREATE INDEX IF NOT EXISTS idx_definitions_qualified ON definitions(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_definitions_kind ON definitions(kind);
        CREATE INDEX IF NOT EXISTS idx_definitions_file ON definitions(file_id);
        CREATE INDEX IF NOT EXISTS idx_refs_name ON refs(name);
        CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);
        CREATE INDEX IF NOT EXISTS idx_refs_def ON refs(def_id);
        CREATE INDEX IF NOT EXISTS idx_call_graph_caller ON call_graph(caller_id);
        CREATE INDEX IF NOT EXISTS idx_call_graph_callee ON call_graph(callee_id);
        CREATE INDEX IF NOT EXISTS idx_inheritance_derived ON inheritance(derived_id);
        CREATE INDEX IF NOT EXISTS idx_inheritance_base ON inheritance(base_id);
        ",
    )
}

/// Persist an [Index] to a SQLite database at `path`.
/// Returns the [Connection] so callers can run further queries.
pub fn save_to_db(index: &Index, path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;",
    )?;
    create_schema(&conn)?;

    let tx = conn.unchecked_transaction()?;

    // Deduplicate and insert files.
    let mut file_ids: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    {
        let mut stmt = tx.prepare(
            "INSERT INTO files (path, language) VALUES (?1, ?2) ON CONFLICT(path) DO UPDATE SET language=excluded.language",
        )?;
        let mut files: HashSet<(&str, &str)> = HashSet::new();
        for d in index.definitions() {
            files.insert((d.location.file.to_str().unwrap_or(""), "c"));
        }
        for r in index.references() {
            files.insert((r.location.file.to_str().unwrap_or(""), "c"));
        }
        for (path_str, lang) in &files {
            stmt.execute(params![path_str, lang])?;
            let id = tx.last_insert_rowid();
            file_ids.insert(path_str.to_string(), id);
        }
    }

    // Insert definitions; build a map from in-memory index → DB id.
    let mut def_id_map: Vec<i64> = Vec::with_capacity(index.definitions().len());
    {
        let mut stmt = tx.prepare(
            "INSERT INTO definitions (name, qualified_name, kind, file_id, line, column, end_line, end_column, parent_id, signature, visibility, is_definition, extra) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        for d in index.definitions() {
            let file_id = d
                .location
                .file
                .to_str()
                .and_then(|p| file_ids.get(p))
                .copied();
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
            def_id_map.push(tx.last_insert_rowid());
        }
    }

    // Insert references, remapping def_id from in-memory index to DB id.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO refs (name, kind, file_id, line, column, def_id, context) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for r in index.references() {
            let file_id = r
                .location
                .file
                .to_str()
                .and_then(|p| file_ids.get(p))
                .copied();
            let db_def_id = r
                .def_id
                .and_then(|idx| def_id_map.get(idx as usize))
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

    // Insert call graph.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO call_graph (caller_name, callee_name, file_id, line, column) VALUES (?1,?2,?3,?4,?5)",
        )?;
        for call in index.calls() {
            let file_id = call
                .location
                .file
                .to_str()
                .and_then(|p| file_ids.get(p))
                .copied();
            stmt.execute(params![
                call.caller_name,
                call.callee_name,
                file_id,
                call.location.line as i64,
                call.location.column as i64,
            ])?;
        }
    }

    // Insert inheritance.
    {
        let mut stmt = tx.prepare(
            "INSERT INTO inheritance (derived_name, base_name, access, is_virtual) VALUES (?1,?2,?3,?4)",
        )?;
        for inh in index.inherits() {
            stmt.execute(params![
                inh.derived_name,
                inh.base_name,
                inh.access.as_str(),
                inh.is_virtual as i64,
            ])?;
        }
    }

    // Resolve caller_id/callee_id via qualified name lookup.
    tx.execute_batch(
        "
        UPDATE call_graph SET caller_id = (SELECT id FROM definitions WHERE qualified_name = call_graph.caller_name LIMIT 1);
        UPDATE call_graph SET callee_id = (SELECT id FROM definitions WHERE qualified_name = call_graph.callee_name LIMIT 1);
        UPDATE inheritance SET derived_id = (SELECT id FROM definitions WHERE qualified_name = inheritance.derived_name LIMIT 1);
        UPDATE inheritance SET base_id = (SELECT id FROM definitions WHERE qualified_name = inheritance.base_name LIMIT 1);
        ",
    )?;

    tx.commit()?;
    // Re-enable foreign keys for future queries.
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// Load an [Index] from a SQLite database at `path`.
/// Returns both the [Index] and the [Connection] for further queries.
pub fn open_from_db(path: &Path) -> rusqlite::Result<(Index, Connection)> {
    let conn = Connection::open(path)?;

    let mut definitions: Vec<Definition> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, name, qualified_name, kind, line, column, end_line, end_column, parent_id, signature, visibility, is_definition, extra, file_id FROM definitions",
        )?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(13)?;
            let file_path = file_id.and_then(|fid| {
                conn.query_row(
                    "SELECT path FROM files WHERE id = ?1",
                    params![fid],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            });
            Ok(Definition {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                qualified_name: row.get(2)?,
                kind: SymbolKind::from_str(&row.get::<_, String>(3)?).unwrap_or(SymbolKind::Function),
                location: Location {
                    file: file_path.map(std::path::PathBuf::from).unwrap_or_default(),
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

    let mut references: Vec<Reference> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, line, column, def_id, context, file_id FROM refs",
        )?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(7)?;
            let file_path = file_id.and_then(|fid| {
                conn.query_row(
                    "SELECT path FROM files WHERE id = ?1",
                    params![fid],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            });
            Ok(Reference {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                kind: SymbolKind::from_str(&row.get::<_, String>(2)?).unwrap_or(SymbolKind::Function),
                location: Location {
                    file: file_path.map(std::path::PathBuf::from).unwrap_or_default(),
                    line: row.get::<_, i64>(3)? as usize,
                    column: row.get::<_, i64>(4)? as usize,
                    end_line: 0,
                    end_column: 0,
                },
                def_id: row.get(5)?,
                context: row.get(6)?,
            })
        })?;
        for r in rows {
            references.push(r.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
        }
    }

    let mut calls: Vec<CallEdge> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT caller_name, callee_name, line, column, file_id FROM call_graph",
        )?;
        let rows = stmt.query_map([], |row| {
            let file_id: Option<i64> = row.get(4)?;
            let file_path = file_id.and_then(|fid| {
                conn.query_row(
                    "SELECT path FROM files WHERE id = ?1",
                    params![fid],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            });
            Ok(CallEdge {
                caller_name: row.get(0)?,
                callee_name: row.get(1)?,
                location: Location {
                    file: file_path.map(std::path::PathBuf::from).unwrap_or_default(),
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
            "SELECT derived_name, base_name, access, is_virtual FROM inheritance",
        )?;
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

    Ok((Index::new(definitions, references, calls, inherits), conn))
}
