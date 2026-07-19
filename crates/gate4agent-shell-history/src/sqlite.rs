use crate::discovery::DiscoveryResult;
use crate::{
    CandidateLocator, DiscoveredCandidate, NativeHistoryDiscoveryIssue,
    NativeHistoryDiscoveryIssueKind, NativeHistoryError, NativeHistoryLimits, NativeHistoryRoot,
};
use gate4agent_adapters::{HistoryDocument, HISTORY_STORED_MESSAGES_MAX};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(crate) fn discover_sessions(
    root: &NativeHistoryRoot,
    root_slot: usize,
    limits: NativeHistoryLimits,
    requested_limit: usize,
) -> DiscoveryResult {
    let mut result = DiscoveryResult {
        candidates: Vec::new(),
        issues: Vec::new(),
    };
    let Ok(metadata) = fs::symlink_metadata(&root.path) else {
        return result;
    };
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::Inaccessible,
        ));
        return result;
    }
    let Ok(canonical_root) = fs::canonicalize(&root.path) else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::Inaccessible,
        ));
        return result;
    };
    let (databases, entry_limit_reached) = if metadata.is_dir() {
        sqlite_files(&canonical_root, limits.max_walk_entries)
    } else {
        (vec![canonical_root], false)
    };
    if entry_limit_reached {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::EntryLimitReached,
        ));
    }
    let candidate_limit = requested_limit.min(limits.max_candidates);
    for database in databases {
        discover_database(root, root_slot, candidate_limit, database, &mut result);
        result
            .candidates
            .sort_by_key(|candidate| std::cmp::Reverse(candidate.modified_at_unix_ms));
        result.candidates.truncate(candidate_limit);
    }
    result
}

fn discover_database(
    root: &NativeHistoryRoot,
    root_slot: usize,
    limit: usize,
    database: std::path::PathBuf,
    result: &mut DiscoveryResult,
) {
    let Ok(connection) = open_readonly(&database) else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::Inaccessible,
        ));
        return;
    };
    let Ok(columns) = table_columns(&connection, "session") else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::InvalidDatabase,
        ));
        return;
    };
    if !["id", "time_created", "time_updated"]
        .into_iter()
        .all(|column| columns.contains(column))
    {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::InvalidDatabase,
        ));
        return;
    }
    let parent_predicate = if columns.contains("parent_id") {
        " AND parent_id IS NULL"
    } else {
        ""
    };
    let archived_predicate = if columns.contains("time_archived") {
        " AND time_archived IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, time_created, time_updated FROM session WHERE 1=1{parent_predicate}{archived_predicate} ORDER BY time_updated DESC LIMIT ?1"
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::InvalidDatabase,
        ));
        return;
    };
    let Ok(rows) = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }) else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::InvalidDatabase,
        ));
        return;
    };
    for row in rows.flatten() {
        let (session_id, created, updated) = row;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            continue;
        }
        let timestamp = if updated > 0 { updated } else { created };
        result.candidates.push(DiscoveredCandidate {
            locator: CandidateLocator::Sqlite {
                database: database.clone(),
                session_id: session_id.to_owned(),
            },
            session_id_hint: session_id.to_owned(),
            modified_at_unix_ms: u64::try_from(timestamp).ok(),
        });
    }
}

fn sqlite_files(directory: &Path, max_entries: usize) -> (Vec<std::path::PathBuf>, bool) {
    let mut entries_seen = 0usize;
    let mut entry_limit_reached = false;
    let mut databases = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .take_while(|_| {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > max_entries {
                entry_limit_reached = true;
                false
            } else {
                true
            }
        })
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if file_type.is_symlink() || !file_type.is_file() {
                return None;
            }
            let name = entry.file_name();
            let name = name.to_str()?;
            let suffix = name.strip_prefix("opencode")?.strip_suffix(".db")?;
            (suffix.is_empty()
                || (suffix.starts_with('-')
                    && suffix[1..].bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
                    })))
            .then(|| fs::canonicalize(entry.path()).ok())
            .flatten()
        })
        .collect::<Vec<_>>();
    databases.sort();
    (databases, entry_limit_reached)
}

pub(crate) fn load_session(
    database: &Path,
    session_id: &str,
) -> Result<HistoryDocument, NativeHistoryError> {
    let mut connection =
        open_readonly(database).map_err(|_| NativeHistoryError::DatabaseUnavailable)?;
    let transaction = connection
        .transaction()
        .map_err(|_| NativeHistoryError::DatabaseUnavailable)?;
    let connection = &*transaction;
    let columns =
        table_columns(connection, "session").map_err(|_| NativeHistoryError::DatabaseSchema)?;
    if !["id", "time_created", "time_updated"]
        .into_iter()
        .all(|column| columns.contains(column))
    {
        return Err(NativeHistoryError::DatabaseSchema);
    }
    let message_columns = table_columns(connection, "message").unwrap_or_default();
    let can_count_messages = ["session_id", "data"]
        .into_iter()
        .all(|column| message_columns.contains(column));
    let text = |column: &str, alias: &str| {
        if columns.contains(column) {
            format!("s.{column} AS {alias}")
        } else {
            format!("NULL AS {alias}")
        }
    };
    let number = |column: &str| {
        if columns.contains(column) {
            format!("s.{column} AS {column}")
        } else {
            format!("0 AS {column}")
        }
    };
    let count = if can_count_messages {
        "(SELECT COUNT(*) FROM message m WHERE m.session_id = s.id AND json_valid(m.data) AND json_extract(m.data, '$.role') IN ('user','assistant'))"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT s.id, {}, {}, {}, {}, {}, {}, {}, {count} AS message_count FROM session s WHERE s.id = ?1 LIMIT 1",
        text("title", "title"),
        text("directory", "directory"),
        text("model", "model_json"),
        number("tokens_input"),
        number("tokens_output"),
        number("tokens_reasoning"),
        number("tokens_cache_read"),
    );
    let row = connection
        .query_row(&sql, [session_id], |row| {
            Ok(SqliteSessionRow {
                id: row.get(0)?,
                title: row.get(1)?,
                directory: row.get(2)?,
                model_json: row.get(3)?,
                tokens_input: nonnegative(row.get::<_, i64>(4)?),
                tokens_output: nonnegative(row.get::<_, i64>(5)?),
                tokens_reasoning: nonnegative(row.get::<_, i64>(6)?),
                tokens_cache_read: nonnegative(row.get::<_, i64>(7)?),
                message_count: nonnegative(row.get::<_, i64>(8)?),
            })
        })
        .optional()
        .map_err(|_| NativeHistoryError::DatabaseSchema)?
        .ok_or(NativeHistoryError::SourceChanged)?;
    if row.id != session_id {
        return Err(NativeHistoryError::SourceChanged);
    }

    let mut metadata = Map::new();
    metadata.insert("id".to_owned(), Value::String(row.id));
    insert_optional_string(&mut metadata, "title", row.title);
    insert_optional_string(&mut metadata, "directory", row.directory);
    insert_optional_string(&mut metadata, "model_json", row.model_json);
    metadata.insert("tokens_input".to_owned(), json!(row.tokens_input));
    metadata.insert("tokens_output".to_owned(), json!(row.tokens_output));
    metadata.insert("tokens_reasoning".to_owned(), json!(row.tokens_reasoning));
    metadata.insert("tokens_cache_read".to_owned(), json!(row.tokens_cache_read));
    metadata.insert("message_count".to_owned(), json!(row.message_count));
    let transcript = load_messages(connection, session_id, &message_columns)?;

    Ok(HistoryDocument {
        session_id_hint: session_id.to_owned(),
        metadata_json: Some(Value::Object(metadata).to_string()),
        transcript,
    })
}

fn load_messages(
    connection: &Connection,
    session_id: &str,
    message_columns: &HashSet<String>,
) -> Result<String, NativeHistoryError> {
    let part_columns = table_columns(connection, "part").unwrap_or_default();
    if !["id", "session_id", "data"]
        .into_iter()
        .all(|column| message_columns.contains(column))
        || !["message_id", "time_created", "data"]
            .into_iter()
            .all(|column| part_columns.contains(column))
    {
        return Ok(String::new());
    }
    let sql = "SELECT json_extract(m.data, '$.role') AS role, p.data AS part_data, json_extract(m.data, '$.summary.title') AS summary_title, json_extract(m.data, '$.summary.body') AS summary_body FROM message m JOIN part p ON p.message_id = m.id WHERE m.session_id = ?1 AND json_valid(m.data) AND json_extract(m.data, '$.role') IN ('user','assistant') AND json_valid(p.data) AND json_extract(p.data, '$.type') = 'text' ORDER BY p.time_created DESC LIMIT ?2";
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| NativeHistoryError::DatabaseSchema)?;
    let rows = statement
        .query_map(
            rusqlite::params![
                session_id,
                i64::try_from(HISTORY_STORED_MESSAGES_MAX).unwrap()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|_| NativeHistoryError::DatabaseSchema)?;
    let mut records = rows
        .filter_map(Result::ok)
        .map(|(role, part_data, title, body)| {
            json!({
                "role": role,
                "part_data": part_data,
                "summary_title": title,
                "summary_body": body,
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    records.reverse();
    Ok(records.join("\n"))
}

fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(connection)
}

fn table_columns(connection: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Ok(HashSet::new());
    }
    let sql = format!("PRAGMA table_info(\"{table}\")");
    let mut statement = connection.prepare(&sql)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .collect();
    Ok(columns)
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn insert_optional_string(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        map.insert(key.to_owned(), Value::String(value));
    }
}

fn issue(
    root: &NativeHistoryRoot,
    root_slot: usize,
    kind: NativeHistoryDiscoveryIssueKind,
) -> NativeHistoryDiscoveryIssue {
    NativeHistoryDiscoveryIssue {
        root_slot,
        adapter_id: root.adapter_id.clone(),
        kind,
    }
}

struct SqliteSessionRow {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    model_json: Option<String>,
    tokens_input: u64,
    tokens_output: u64,
    tokens_reasoning: u64,
    tokens_cache_read: u64,
    message_count: u64,
}
