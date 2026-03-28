use hearken_core::LogSource;
use rusqlite::{params, Connection, OpenFlags, Result as RusqliteResult};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Database error: {0}")]
    DbError(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub struct Storage {
    pub conn: Connection,
}

impl Storage {
    pub fn open(path: &str) -> std::result::Result<Self, StorageError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = OFF;
             PRAGMA cache_size = -1000000;
             PRAGMA temp_store = MEMORY;
             PRAGMA locking_mode = EXCLUSIVE;
             PRAGMA page_size = 16384;"
        )?;

        let storage = Self { conn };
        storage.init_schema()?;

        Ok(storage)
    }

    fn init_schema(&self) -> RusqliteResult<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS file_groups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL
            );

            CREATE TABLE IF NOT EXISTS log_sources (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT UNIQUE NOT NULL,
                file_group_id INTEGER NOT NULL,
                last_processed_position INTEGER DEFAULT 0,
                file_hash TEXT,
                FOREIGN KEY(file_group_id) REFERENCES file_groups(id)
            );

            CREATE TABLE IF NOT EXISTS patterns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_group_id INTEGER NOT NULL,
                template TEXT NOT NULL,
                occurrence_count INTEGER DEFAULT 0,
                FOREIGN KEY(file_group_id) REFERENCES file_groups(id),
                UNIQUE(file_group_id, template)
            );

            CREATE TABLE IF NOT EXISTS occurrences (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                log_source_id INTEGER NOT NULL,
                pattern_id INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                variables TEXT,
                FOREIGN KEY(log_source_id) REFERENCES log_sources(id),
                FOREIGN KEY(pattern_id) REFERENCES patterns(id)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS patterns_fts USING fts5(
                pattern_id UNINDEXED,
                template
            );

            CREATE INDEX IF NOT EXISTS idx_occ_pattern ON occurrences(pattern_id);
            CREATE INDEX IF NOT EXISTS idx_occ_source ON occurrences(log_source_id);
            CREATE INDEX IF NOT EXISTS idx_patterns_group ON patterns(file_group_id);
            ",
        )?;
        Ok(())
    }

    pub fn get_or_create_file_group(&self, name: &str) -> Result<i64, StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO file_groups (name) VALUES (?)",
            params![name],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM file_groups WHERE name = ?",
            params![name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_or_create_log_source(&self, path: &str, file_group_id: i64) -> Result<LogSource, StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO log_sources (file_path, file_group_id) VALUES (?, ?)",
            params![path, file_group_id],
        )?;

        let mut stmt = self.conn.prepare("SELECT id, file_path, last_processed_position, file_hash FROM log_sources WHERE file_path = ?")?;
        let source = stmt.query_row(params![path], |row| {
            let last_pos: i64 = row.get(2)?;
            Ok(LogSource {
                id: Some(row.get(0)?),
                file_path: row.get(1)?,
                last_processed_position: last_pos as u64,
                file_hash: row.get(3)?,
            })
        })?;
        Ok(source)
    }

    pub fn search_patterns(&self, query: &str) -> Result<Vec<(i64, String)>, StorageError> {
        let mut stmt = self.conn.prepare("SELECT pattern_id, template FROM patterns_fts WHERE patterns_fts MATCH ?")?;
        let rows = stmt.query_map(params![query], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_top_patterns(&self, limit: usize) -> Result<Vec<(String, i64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT template, occurrence_count
             FROM patterns
             WHERE occurrence_count > 0
             ORDER BY occurrence_count DESC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_all_patterns_ranked(
        &self,
        limit: usize,
        filter: Option<&[String]>,
        group_filter: Option<&[String]>,
    ) -> Result<Vec<(i64, String, i64, String)>, StorageError> {
        let mut conditions = vec!["p.occurrence_count > 0".to_string()];
        let mut bind_values: Vec<String> = Vec::new();

        if let Some(terms) = filter {
            if !terms.is_empty() {
                let like_conds: Vec<String> = terms.iter()
                    .map(|_| "p.template LIKE ?".to_string())
                    .collect();
                conditions.push(format!("({})", like_conds.join(" OR ")));
                bind_values.extend(terms.iter().map(|t| format!("%{}%", t)));
            }
        }

        if let Some(groups) = group_filter {
            if !groups.is_empty() {
                let placeholders: Vec<String> = groups.iter().map(|_| "?".to_string()).collect();
                conditions.push(format!("fg.name IN ({})", placeholders.join(", ")));
                bind_values.extend(groups.iter().cloned());
            }
        }

        let sql = format!(
            "SELECT p.id, p.template, p.occurrence_count, fg.name \
             FROM patterns p \
             JOIN file_groups fg ON p.file_group_id = fg.id \
             WHERE {} \
             ORDER BY p.occurrence_count DESC LIMIT ?",
            conditions.join(" AND ")
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = bind_values
            .iter()
            .map(|v| Box::new(v.clone()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        params_vec.push(Box::new(limit as i64));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter()
            .map(|p| p.as_ref())
            .collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_pattern_samples(
        &self,
        pattern_id: i64,
        limit: usize,
    ) -> Result<Vec<(String, String)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT o.variables, ls.file_path FROM occurrences o
             JOIN log_sources ls ON o.log_source_id = ls.id
             WHERE o.pattern_id = ? AND o.variables IS NOT NULL AND o.variables != ''
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![pattern_id, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_report_summary(&self) -> Result<(i64, i64, Vec<String>, Vec<(String, i64)>), StorageError> {
        let pattern_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM patterns", [], |row| row.get(0),
        )?;
        let total_occurrences: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(occurrence_count), 0) FROM patterns", [], |row| row.get(0),
        )?;
        let mut stmt = self.conn.prepare("SELECT file_path FROM log_sources")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut sources = Vec::new();
        for row in rows { sources.push(row?); }

        let mut stmt = self.conn.prepare(
            "SELECT fg.name, COUNT(p.id) FROM file_groups fg \
             LEFT JOIN patterns p ON p.file_group_id = fg.id AND p.occurrence_count > 0 \
             GROUP BY fg.id ORDER BY fg.name"
        )?;
        let group_rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut groups = Vec::new();
        for row in group_rows { groups.push(row?); }

        Ok((pattern_count, total_occurrences, sources, groups))
    }

    /// Returns per-source occurrence counts for given pattern IDs.
    /// Returns: HashMap<pattern_id, Vec<(source_file_name, count)>> sorted by source path.
    pub fn get_pattern_trends(
        &self,
        pattern_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<(String, i64)>>, StorageError> {
        if pattern_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut result: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
        // SQLite has a variable limit (~999), so batch
        for chunk in pattern_ids.chunks(500) {
            let placeholders: Vec<String> = chunk.iter().map(|_| "?".to_string()).collect();
            let sql = format!(
                "SELECT o.pattern_id, ls.file_path, COUNT(*) as cnt
                 FROM occurrences o
                 JOIN log_sources ls ON o.log_source_id = ls.id
                 WHERE o.pattern_id IN ({})
                 GROUP BY o.pattern_id, o.log_source_id
                 ORDER BY o.pattern_id, ls.file_path",
                placeholders.join(", ")
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = chunk.iter()
                .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
                .collect();
            let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter()
                .map(|p| p.as_ref())
                .collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
            })?;
            for row in rows {
                let (pid, path, cnt) = row?;
                let file_name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&path)
                    .to_string();
                result.entry(pid).or_default().push((file_name, cnt));
            }
        }
        Ok(result)
    }

    /// Get all patterns (id, template, occurrence_count, group_name) optionally filtered by group.
    pub fn get_patterns_for_dedup(
        &self,
        group_filter: Option<&str>,
    ) -> Result<Vec<(i64, String, i64, String)>, StorageError> {
        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(g) = group_filter {
            (
                "SELECT p.id, p.template, p.occurrence_count, fg.name \
                 FROM patterns p JOIN file_groups fg ON p.file_group_id = fg.id \
                 WHERE fg.name = ? ORDER BY p.occurrence_count DESC".to_string(),
                vec![Box::new(g.to_string()) as Box<dyn rusqlite::types::ToSql>],
            )
        } else {
            (
                "SELECT p.id, p.template, p.occurrence_count, fg.name \
                 FROM patterns p JOIN file_groups fg ON p.file_group_id = fg.id \
                 ORDER BY fg.name, p.occurrence_count DESC".to_string(),
                vec![],
            )
        };
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }
    /// Count source files per file group.
    pub fn get_source_counts_per_group(&self) -> Result<HashMap<String, usize>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT fg.name, COUNT(ls.id) FROM log_sources ls \
             JOIN file_groups fg ON ls.file_group_id = fg.id \
             GROUP BY fg.name"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        let mut result = HashMap::new();
        for row in rows { let (g, c) = row?; result.insert(g, c); }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_full_flow() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("full_flow.db");
        let path_str = db_path.to_str().unwrap();

        let storage = Storage::open(path_str).unwrap();
        let group_id = storage.get_or_create_file_group("test.log").unwrap();
        let _source = storage.get_or_create_log_source("test.log", group_id).unwrap();
        
        storage.conn.execute(
            "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, 'User <*> logged in', 42)",
            params![group_id],
        ).unwrap();
        let pattern_id = storage.conn.last_insert_rowid();
        storage.conn.execute("INSERT INTO patterns_fts (pattern_id, template) VALUES (?, 'User <*> logged in')", params![pattern_id]).unwrap();

        let search_results = storage.search_patterns("User").unwrap();
        assert_eq!(search_results.len(), 1);

        let top = storage.get_top_patterns(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1, 42);
    }
}
