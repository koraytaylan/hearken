pub mod client;
pub mod filter;
pub mod mapper;

use client::{JiraClient, JiraIssue};
use filter::{FilterOptions, filter_patterns};
use mapper::{HearkenMarker, parse_marker};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JiraInstanceType {
    Cloud,
    Server,
}

impl fmt::Display for JiraInstanceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JiraInstanceType::Cloud => write!(f, "cloud"),
            JiraInstanceType::Server => write!(f, "server"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraTomlConfig {
    pub url: String,
    pub project: String,
    pub label: String,
    #[serde(rename = "type")]
    pub instance_type: JiraInstanceType,
    pub issue_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JiraConfig {
    pub url: String,
    pub project: String,
    pub label: String,
    pub instance_type: JiraInstanceType,
    pub issue_type: String,
    pub user: String,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JiraError {
    #[error("JIRA configuration error: {0}")]
    Config(String),
    #[error("JIRA API error: {status} - {message}")]
    Api { status: u16, message: String },
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Storage error: {0}")]
    Storage(#[from] hearken_storage::StorageError),
}

impl JiraConfig {
    pub fn from_toml_and_env(toml: JiraTomlConfig) -> Result<Self, JiraError> {
        let user = std::env::var("HEARKEN_JIRA_USER").map_err(|_| {
            JiraError::Config("HEARKEN_JIRA_USER environment variable not set".into())
        })?;
        let token = std::env::var("HEARKEN_JIRA_TOKEN").map_err(|_| {
            JiraError::Config("HEARKEN_JIRA_TOKEN environment variable not set".into())
        })?;
        let url = toml.url.trim_end_matches('/').to_string();
        Ok(Self {
            url,
            project: toml.project,
            label: toml.label,
            instance_type: toml.instance_type,
            issue_type: toml.issue_type.unwrap_or_else(|| "Bug".to_string()),
            user,
            token,
        })
    }
}

// ---------------------------------------------------------------------------
// SyncResult
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SyncResult {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: usize,
    pub failed: Vec<(String, String)>,
}

impl SyncResult {
    pub fn print_summary(&self) {
        let total = self.created.len() + self.updated.len() + self.unchanged + self.failed.len();
        println!(
            "Synced {} patterns ({} created, {} updated, {} unchanged, {} failed)",
            total,
            self.created.len(),
            self.updated.len(),
            self.unchanged,
            self.failed.len(),
        );
        for key in &self.created {
            println!("  + Created: {}", key);
        }
        for key in &self.updated {
            println!("  ~ Updated: {}", key);
        }
        for (tmpl, err) in &self.failed {
            eprintln!("  ! Failed: {} — {}", truncate_preview(tmpl), err);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: extract plain text from a JiraIssue description (String or ADF JSON)
// ---------------------------------------------------------------------------

pub fn extract_description_text(issue: &JiraIssue) -> String {
    match &issue.fields.description {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => extract_adf_text(v),
    }
}

pub fn extract_adf_text(node: &serde_json::Value) -> String {
    let mut result = String::new();

    // Reconstruct {code:title=hearken-metadata} wrapper for ADF codeBlock nodes
    // so that parse_marker() can find the marker inside the expected block.
    let is_metadata_block = node.get("type").and_then(|t| t.as_str()) == Some("codeBlock")
        && node
            .get("attrs")
            .and_then(|a| a.get("language"))
            .and_then(|l| l.as_str())
            == Some("hearken-metadata");

    if is_metadata_block {
        result.push_str("{code:title=hearken-metadata}\n");
    }

    if let Some(text) = node.get("text").and_then(|t| t.as_str()) {
        result.push_str(text);
        result.push('\n');
    }
    if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
        for child in content {
            result.push_str(&extract_adf_text(child));
        }
    }

    if is_metadata_block {
        result.push_str("{code}\n");
    }

    result
}

// ---------------------------------------------------------------------------
// Helper: build ticket map keyed by (db_name, pattern_id)
// ---------------------------------------------------------------------------

async fn build_ticket_map(
    client: &JiraClient,
) -> Result<HashMap<(String, i64), (JiraIssue, HearkenMarker)>, JiraError> {
    let issues = client.fetch_hearken_tickets().await?;
    let mut map = HashMap::new();
    for issue in issues {
        let text = extract_description_text(&issue);
        if let Some(marker) = parse_marker(&text) {
            let key = (marker.db.clone(), marker.pattern_id);
            map.insert(key, (issue, marker));
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Helper: get MIN/MAX entry_timestamp for a pattern
// ---------------------------------------------------------------------------

pub fn get_pattern_timestamps(
    storage: &hearken_storage::Storage,
    pattern_id: i64,
) -> (Option<String>, Option<String>) {
    let result: rusqlite::Result<(Option<i64>, Option<i64>)> = storage.conn.query_row(
        "SELECT MIN(entry_timestamp), MAX(entry_timestamp) FROM occurrences WHERE pattern_id = ? AND entry_timestamp IS NOT NULL",
        rusqlite::params![pattern_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );
    match result {
        Ok((Some(min_ts), Some(max_ts))) => {
            let fmt_ts = |ts: i64| {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| ts.to_string())
            };
            (Some(fmt_ts(min_ts)), Some(fmt_ts(max_ts)))
        }
        _ => (None, None),
    }
}

// ---------------------------------------------------------------------------
// Helper: get sample log lines for a pattern
// ---------------------------------------------------------------------------

pub fn get_sample_lines(
    storage: &hearken_storage::Storage,
    pattern_id: i64,
    limit: usize,
) -> Vec<String> {
    match storage.get_pattern_samples(pattern_id, limit) {
        Ok(samples) => samples
            .into_iter()
            .map(|(vars, _file)| vars.replace('\t', " | "))
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public async: sync()
// ---------------------------------------------------------------------------

pub async fn sync(
    config: JiraConfig,
    storage: &hearken_storage::Storage,
    db_name: &str,
    filter_opts: &FilterOptions,
    anomaly_ids: Option<&HashSet<i64>>,
    dry_run: bool,
) -> Result<SyncResult, JiraError> {
    let client = JiraClient::new(config.clone())?;
    let ticket_map = build_ticket_map(&client).await?;

    // Build set of already-synced pattern IDs for this db
    let synced_ids: HashSet<i64> = ticket_map
        .keys()
        .filter(|(db, _)| db == db_name)
        .map(|(_, pid)| *pid)
        .collect();

    let patterns = filter_patterns(storage, filter_opts, &synced_ids, anomaly_ids)?;

    let mut result = SyncResult::default();

    for pattern in patterns {
        let key = (db_name.to_string(), pattern.id);
        let (first_seen, last_seen) = get_pattern_timestamps(storage, pattern.id);
        let samples = get_sample_lines(storage, pattern.id, 5);

        let input = mapper::TicketBodyInput {
            template: pattern.template.clone(),
            occurrence_count: pattern.occurrence_count,
            first_seen: first_seen.clone(),
            last_seen: last_seen.clone(),
            file_group: pattern.file_group.clone(),
            samples,
            db_name: db_name.to_string(),
            pattern_id: pattern.id,
        };

        if let Some((existing_issue, marker)) = ticket_map.get(&key) {
            // Ticket exists — check if occurrences changed
            if marker.occurrences == pattern.occurrence_count {
                result.unchanged += 1;
                continue;
            }

            // Occurrences changed: update description + add comment
            let new_description = match config.instance_type {
                JiraInstanceType::Server => {
                    serde_json::Value::String(mapper::build_description_wiki(&input))
                }
                JiraInstanceType::Cloud => mapper::build_description_adf(&input),
            };

            let last_seen_str = last_seen.as_deref().unwrap_or("unknown");

            let comment_body = match config.instance_type {
                JiraInstanceType::Server => {
                    serde_json::Value::String(mapper::build_change_comment_wiki(
                        marker.occurrences,
                        pattern.occurrence_count,
                        last_seen_str,
                    ))
                }
                JiraInstanceType::Cloud => mapper::build_change_comment_adf(
                    marker.occurrences,
                    pattern.occurrence_count,
                    last_seen_str,
                ),
            };

            if dry_run {
                println!(
                    "  [dry-run] Would update: {} (occurrences {} -> {})",
                    existing_issue.key, marker.occurrences, pattern.occurrence_count
                );
                result.updated.push(existing_issue.key.clone());
                continue;
            }

            let issue_key = existing_issue.key.clone();

            if let Err(e) = client
                .update_issue_description(&issue_key, new_description)
                .await
            {
                let preview = truncate_preview(&pattern.template);
                result.failed.push((preview, e.to_string()));
                continue;
            }

            if let Err(e) = client.add_comment(&issue_key, comment_body).await {
                let preview = truncate_preview(&pattern.template);
                result.failed.push((preview, e.to_string()));
                continue;
            }

            result.updated.push(issue_key);
        } else {
            // No existing ticket — create new one
            let summary = mapper::build_summary(&pattern.template);

            let description = match config.instance_type {
                JiraInstanceType::Server => {
                    serde_json::Value::String(mapper::build_description_wiki(&input))
                }
                JiraInstanceType::Cloud => mapper::build_description_adf(&input),
            };

            if dry_run {
                println!("  [dry-run] Would create ticket for: {}", &summary);
                result.created.push(format!("[dry-run] {}", &summary));
                continue;
            }

            match client
                .create_issue(&summary, description, &config.label, &config.issue_type)
                .await
            {
                Ok(key) => result.created.push(key),
                Err(e) => {
                    let preview = truncate_preview(&pattern.template);
                    result.failed.push((preview, e.to_string()));
                }
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Public async: update() — same as sync() but never creates new tickets
// ---------------------------------------------------------------------------

pub async fn update(
    config: JiraConfig,
    storage: &hearken_storage::Storage,
    db_name: &str,
    filter_opts: &FilterOptions,
    anomaly_ids: Option<&HashSet<i64>>,
    dry_run: bool,
) -> Result<SyncResult, JiraError> {
    let client = JiraClient::new(config.clone())?;
    let ticket_map = build_ticket_map(&client).await?;

    let synced_ids: HashSet<i64> = ticket_map
        .keys()
        .filter(|(db, _)| db == db_name)
        .map(|(_, pid)| *pid)
        .collect();

    let patterns = filter_patterns(storage, filter_opts, &synced_ids, anomaly_ids)?;

    let mut result = SyncResult::default();

    for pattern in patterns {
        let key = (db_name.to_string(), pattern.id);

        let Some((existing_issue, marker)) = ticket_map.get(&key) else {
            // update() skips patterns that have no existing ticket
            continue;
        };

        if marker.occurrences == pattern.occurrence_count {
            result.unchanged += 1;
            continue;
        }

        let (first_seen, last_seen) = get_pattern_timestamps(storage, pattern.id);
        let samples = get_sample_lines(storage, pattern.id, 5);

        let input = mapper::TicketBodyInput {
            template: pattern.template.clone(),
            occurrence_count: pattern.occurrence_count,
            first_seen,
            last_seen: last_seen.clone(),
            file_group: pattern.file_group.clone(),
            samples,
            db_name: db_name.to_string(),
            pattern_id: pattern.id,
        };

        let new_description = match config.instance_type {
            JiraInstanceType::Server => {
                serde_json::Value::String(mapper::build_description_wiki(&input))
            }
            JiraInstanceType::Cloud => mapper::build_description_adf(&input),
        };

        let last_seen_str = last_seen.as_deref().unwrap_or("unknown");

        let comment_body = match config.instance_type {
            JiraInstanceType::Server => {
                serde_json::Value::String(mapper::build_change_comment_wiki(
                    marker.occurrences,
                    pattern.occurrence_count,
                    last_seen_str,
                ))
            }
            JiraInstanceType::Cloud => mapper::build_change_comment_adf(
                marker.occurrences,
                pattern.occurrence_count,
                last_seen_str,
            ),
        };

        if dry_run {
            println!(
                "  [dry-run] Would update: {} (occurrences {} -> {})",
                existing_issue.key, marker.occurrences, pattern.occurrence_count
            );
            result.updated.push(existing_issue.key.clone());
            continue;
        }

        let issue_key = existing_issue.key.clone();

        if let Err(e) = client
            .update_issue_description(&issue_key, new_description)
            .await
        {
            let preview = truncate_preview(&pattern.template);
            result.failed.push((preview, e.to_string()));
            continue;
        }

        if let Err(e) = client.add_comment(&issue_key, comment_body).await {
            let preview = truncate_preview(&pattern.template);
            result.failed.push((preview, e.to_string()));
            continue;
        }

        result.updated.push(issue_key);
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Public async: status()
// ---------------------------------------------------------------------------

pub async fn status(
    config: JiraConfig,
    storage: &hearken_storage::Storage,
    db_name: &str,
) -> Result<(), JiraError> {
    let client = JiraClient::new(config)?;
    let ticket_map = build_ticket_map(&client).await?;

    // Total patterns in storage
    let total_patterns: i64 = storage
        .conn
        .query_row(
            "SELECT COUNT(*) FROM patterns WHERE occurrence_count > 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Patterns that have a JIRA ticket for this db
    let with_tickets: usize = ticket_map.keys().filter(|(db, _)| db == db_name).count();

    // Patterns without a ticket (new / unsynced)
    let new_count = (total_patterns as usize).saturating_sub(with_tickets);

    // Patterns whose occurrence count has changed since last sync
    let changed_count = ticket_map
        .values()
        .filter(|(_, marker)| {
            if marker.db != db_name {
                return false;
            }
            // Look up the current occurrence count from storage
            let current: rusqlite::Result<i64> = storage.conn.query_row(
                "SELECT occurrence_count FROM patterns WHERE id = ?",
                rusqlite::params![marker.pattern_id],
                |row| row.get(0),
            );
            match current {
                Ok(count) => count != marker.occurrences,
                Err(_) => false,
            }
        })
        .count();

    println!("JIRA sync status for db: {}", db_name);
    println!("  Total patterns (storage):  {}", total_patterns);
    println!("  Synced (have JIRA ticket): {}", with_tickets);
    println!("  New (not yet synced):      {}", new_count);
    println!("  Changed (need update):     {}", changed_count);

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helper: truncate template for error preview
// ---------------------------------------------------------------------------

fn truncate_preview(template: &str) -> String {
    if template.len() > 60 {
        let mut end = 60;
        while end > 0 && !template.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &template[..end])
    } else {
        template.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jira_toml_config_deserialize() {
        let toml_str = r#"
            url = "https://mycompany.atlassian.net"
            project = "OPS"
            label = "hearken"
            type = "cloud"
        "#;
        let config: JiraTomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.url, "https://mycompany.atlassian.net");
        assert_eq!(config.project, "OPS");
        assert_eq!(config.label, "hearken");
        assert_eq!(config.instance_type, JiraInstanceType::Cloud);
        assert_eq!(config.issue_type, None);
    }

    #[test]
    fn test_jira_toml_config_server() {
        let toml_str = r#"
            url = "https://jira.internal.com"
            project = "INFRA"
            label = "hearken-logs"
            type = "server"
            issue_type = "Task"
        "#;
        let config: JiraTomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.instance_type, JiraInstanceType::Server);
        assert_eq!(config.issue_type, Some("Task".to_string()));
    }

    /// Combined into one test to avoid env var race conditions across parallel
    /// test threads (env vars are process-global).
    #[test]
    fn test_jira_config_env_vars() {
        // Part 1: missing env vars → error
        unsafe {
            std::env::remove_var("HEARKEN_JIRA_USER");
            std::env::remove_var("HEARKEN_JIRA_TOKEN");
        }
        let toml_missing = JiraTomlConfig {
            url: "https://myco.atlassian.net".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let err = JiraConfig::from_toml_and_env(toml_missing).unwrap_err();
        assert!(err.to_string().contains("HEARKEN_JIRA_USER"));

        // Part 2: set env vars → success
        unsafe {
            std::env::set_var("HEARKEN_JIRA_USER", "test@example.com");
            std::env::set_var("HEARKEN_JIRA_TOKEN", "secret-token");
        }
        let toml_ok = JiraTomlConfig {
            url: "https://myco.atlassian.net/".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let config = JiraConfig::from_toml_and_env(toml_ok).unwrap();
        assert_eq!(config.url, "https://myco.atlassian.net");
        assert_eq!(config.issue_type, "Bug");
        assert_eq!(config.user, "test@example.com");
        assert_eq!(config.token, "secret-token");
        unsafe {
            std::env::remove_var("HEARKEN_JIRA_USER");
            std::env::remove_var("HEARKEN_JIRA_TOKEN");
        }
    }
}
