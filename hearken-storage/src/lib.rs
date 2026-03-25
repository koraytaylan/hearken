use hearken_core::LogSource;
use rusqlite::{params, Connection, OpenFlags, Result as RusqliteResult};
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
            CREATE TABLE IF NOT EXISTS log_sources (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT UNIQUE NOT NULL,
                last_processed_position INTEGER DEFAULT 0,
                file_hash TEXT
            );

            CREATE TABLE IF NOT EXISTS patterns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                template TEXT UNIQUE NOT NULL,
                occurrence_count INTEGER DEFAULT 0
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
            ",
        )?;
        Ok(())
    }

    pub fn get_or_create_log_source(&self, path: &str) -> Result<LogSource, StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO log_sources (file_path) VALUES (?)",
            params![path],
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
    ) -> Result<Vec<(i64, String, i64)>, StorageError> {
        let (sql, bind_values) = match filter {
            Some(terms) if !terms.is_empty() => {
                let conditions: Vec<String> = terms.iter()
                    .map(|_| "template LIKE ?".to_string())
                    .collect();
                let where_clause = conditions.join(" OR ");
                let sql = format!(
                    "SELECT id, template, occurrence_count FROM patterns \
                     WHERE occurrence_count > 0 AND ({}) \
                     ORDER BY occurrence_count DESC LIMIT ?",
                    where_clause
                );
                let values: Vec<String> = terms.iter()
                    .map(|t| format!("%{}%", t))
                    .collect();
                (sql, values)
            }
            _ => {
                let sql = "SELECT id, template, occurrence_count FROM patterns \
                           WHERE occurrence_count > 0 \
                           ORDER BY occurrence_count DESC LIMIT ?".to_string();
                (sql, Vec::new())
            }
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let _param_count = bind_values.len();
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = bind_values
            .iter()
            .map(|v| Box::new(v.clone()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        params_vec.push(Box::new(limit as i64));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter()
            .map(|p| p.as_ref())
            .collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_pattern_samples(
        &self,
        pattern_id: i64,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT variables FROM occurrences
             WHERE pattern_id = ? AND variables IS NOT NULL AND variables != ''
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![pattern_id, limit as i64], |row| {
            row.get::<_, String>(0)
        })?;
        let mut results = Vec::new();
        for row in rows { results.push(row?); }
        Ok(results)
    }

    pub fn get_report_summary(&self) -> Result<(i64, i64, Vec<String>), StorageError> {
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
        Ok((pattern_count, total_occurrences, sources))
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
        let _source = storage.get_or_create_log_source("test.log").unwrap();
        
        storage.conn.execute("INSERT INTO patterns (template, occurrence_count) VALUES ('User <*> logged in', 42)", []).unwrap();
        let pattern_id = storage.conn.last_insert_rowid();
        storage.conn.execute("INSERT INTO patterns_fts (pattern_id, template) VALUES (?, 'User <*> logged in')", params![pattern_id]).unwrap();

        let search_results = storage.search_patterns("User").unwrap();
        assert_eq!(search_results.len(), 1);

        let top = storage.get_top_patterns(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1, 42);
    }
}
