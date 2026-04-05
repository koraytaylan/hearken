use hearken_storage::Storage;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct FilterOptions {
    pub anomalies_only: bool,
    pub tags: Option<Vec<String>>,
    pub exclude_tags: Option<Vec<String>>,
    pub min_occurrences: Option<i64>,
    pub new_only: bool,
}

#[derive(Debug, Clone)]
pub struct FilteredPattern {
    pub id: i64,
    pub template: String,
    pub occurrence_count: i64,
    pub file_group: String,
}

pub fn filter_patterns(
    storage: &Storage,
    options: &FilterOptions,
    synced_pattern_ids: &HashSet<i64>,
    anomaly_ids: Option<&HashSet<i64>>,
) -> Result<Vec<FilteredPattern>, hearken_storage::StorageError> {
    let all_patterns = storage.get_all_patterns_ranked(usize::MAX, None, None)?;

    let need_tags = options.tags.is_some() || options.exclude_tags.is_some();
    let tag_map: HashMap<i64, Vec<String>> = if need_tags {
        storage.get_all_tags()?
    } else {
        HashMap::new()
    };

    let mut results = Vec::new();

    for (id, template, count, group) in all_patterns {
        // Filter: anomalies_only
        if options.anomalies_only
            && let Some(ids) = anomaly_ids
            && !ids.contains(&id)
        {
            continue;
        }

        // Filter: min_occurrences
        if let Some(min) = options.min_occurrences
            && count < min
        {
            continue;
        }

        // Filter: tags (must have at least one matching tag)
        if let Some(required_tags) = &options.tags {
            let pattern_tags = tag_map.get(&id).map(|v| v.as_slice()).unwrap_or(&[]);
            let has_match = required_tags
                .iter()
                .any(|rt| pattern_tags.iter().any(|pt| pt == rt));
            if !has_match {
                continue;
            }
        }

        // Filter: exclude_tags (skip if pattern has any excluded tag)
        if let Some(excluded_tags) = &options.exclude_tags {
            let pattern_tags = tag_map.get(&id).map(|v| v.as_slice()).unwrap_or(&[]);
            let has_excluded = excluded_tags
                .iter()
                .any(|et| pattern_tags.iter().any(|pt| pt == et));
            if has_excluded {
                continue;
            }
        }

        // Filter: new_only (skip if already synced)
        if options.new_only && synced_pattern_ids.contains(&id) {
            continue;
        }

        results.push(FilteredPattern {
            id,
            template,
            occurrence_count: count,
            file_group: group,
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearken_storage::Storage;
    use rusqlite::params;
    use tempfile::TempDir;

    fn setup_db() -> (Storage, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(db_path.to_str().unwrap()).unwrap();

        let group_id = storage.get_or_create_file_group("app.log").unwrap();

        // Insert 4 patterns
        storage
            .conn
            .execute(
                "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, ?, ?)",
                params![group_id, "ERROR user <*> login failed", 100i64],
            )
            .unwrap();
        let id1 = storage.conn.last_insert_rowid();

        storage
            .conn
            .execute(
                "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, ?, ?)",
                params![group_id, "WARN timeout after <*>ms", 50i64],
            )
            .unwrap();

        storage
            .conn
            .execute(
                "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, ?, ?)",
                params![group_id, "INFO request completed in <*>ms", 500i64],
            )
            .unwrap();

        storage
            .conn
            .execute(
                "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, ?, ?)",
                params![group_id, "ERROR database connection lost", 10i64],
            )
            .unwrap();
        let id4 = storage.conn.last_insert_rowid();

        // Tag pattern 1 with "critical"
        storage.add_tag(id1, "critical").unwrap();
        // Tag pattern 4 with "suppressed"
        storage.add_tag(id4, "suppressed").unwrap();

        (storage, dir)
    }

    #[test]
    fn test_filter_no_filters() {
        let (storage, _dir) = setup_db();
        let options = FilterOptions::default();
        let synced = HashSet::new();
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_filter_min_occurrences() {
        let (storage, _dir) = setup_db();
        let options = FilterOptions {
            min_occurrences: Some(50),
            ..Default::default()
        };
        let synced = HashSet::new();
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 3);
        // counts should be 500, 100, 50 (sorted by occurrence_count DESC)
        let counts: Vec<i64> = results.iter().map(|p| p.occurrence_count).collect();
        assert!(counts.contains(&100));
        assert!(counts.contains(&50));
        assert!(counts.contains(&500));
    }

    #[test]
    fn test_filter_by_tags() {
        let (storage, _dir) = setup_db();
        let options = FilterOptions {
            tags: Some(vec!["critical".to_string()]),
            ..Default::default()
        };
        let synced = HashSet::new();
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].template.contains("login failed"));
    }

    #[test]
    fn test_filter_exclude_tags() {
        let (storage, _dir) = setup_db();
        let options = FilterOptions {
            exclude_tags: Some(vec!["suppressed".to_string()]),
            ..Default::default()
        };
        let synced = HashSet::new();
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 3);
        assert!(
            !results
                .iter()
                .any(|p| p.template.contains("database connection lost"))
        );
    }

    #[test]
    fn test_filter_new_only() {
        let (storage, _dir) = setup_db();
        // Determine the IDs of patterns 1 and 3 by querying
        let all_options = FilterOptions::default();
        let empty_synced: HashSet<i64> = HashSet::new();
        let all = filter_patterns(&storage, &all_options, &empty_synced, None).unwrap();

        // Find IDs for patterns matching login failed (1) and request completed (3)
        let id1 = all
            .iter()
            .find(|p| p.template.contains("login failed"))
            .unwrap()
            .id;
        let id3 = all
            .iter()
            .find(|p| p.template.contains("request completed"))
            .unwrap()
            .id;

        let mut synced: HashSet<i64> = HashSet::new();
        synced.insert(id1);
        synced.insert(id3);

        let options = FilterOptions {
            new_only: true,
            ..Default::default()
        };
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 2);
        assert!(!results.iter().any(|p| p.id == id1));
        assert!(!results.iter().any(|p| p.id == id3));
    }

    #[test]
    fn test_filter_combined() {
        let (storage, _dir) = setup_db();
        let options = FilterOptions {
            min_occurrences: Some(50),
            exclude_tags: Some(vec!["suppressed".to_string()]),
            ..Default::default()
        };
        let synced = HashSet::new();
        let results = filter_patterns(&storage, &options, &synced, None).unwrap();
        assert_eq!(results.len(), 3);
        // Should have: login failed (100), timeout (50), request completed (500)
        // Should NOT have: database connection lost (10, also suppressed)
        assert!(
            !results
                .iter()
                .any(|p| p.template.contains("database connection lost"))
        );
    }
}
