# JIRA Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `hearken-jira` crate behind a feature flag that enables creating/updating JIRA tickets from discovered log patterns, with CLI subcommands and inline integration on `process`/`watch`.

**Architecture:** New `hearken-jira` workspace crate with four modules: `client.rs` (HTTP/auth), `mapper.rs` (ticket body generation + marker parsing), `filter.rs` (pattern filtering), and `lib.rs` (public sync/status orchestration). `hearken-cli` depends on it optionally via `jira` feature flag, mirroring the existing `web` feature pattern.

**Tech Stack:** Rust 2024 edition, reqwest (HTTP client), serde/serde_json (serialization), hearken-core + hearken-storage (data access)

---

## File Structure

| File | Responsibility |
|------|----------------|
| `hearken-jira/Cargo.toml` | Crate manifest with reqwest, serde, hearken-core, hearken-storage deps |
| `hearken-jira/src/lib.rs` | Public API: `JiraConfig`, `JiraInstanceType`, `SyncOptions`, `SyncResult`, `sync()`, `update()`, `status()` |
| `hearken-jira/src/client.rs` | `JiraClient` struct: HTTP requests, auth, pagination, rate limiting, Cloud/Server dispatch |
| `hearken-jira/src/mapper.rs` | Ticket body generation (ADF + wiki markup), marker embedding/parsing, change detection |
| `hearken-jira/src/filter.rs` | `FilterOptions` struct and `filter_patterns()`: anomalies, tags, thresholds, new-only |
| `Cargo.toml` (workspace root) | Add `hearken-jira` to workspace members |
| `hearken-cli/Cargo.toml` | Add `jira` feature flag and optional `hearken-jira` dependency |
| `hearken-cli/src/main.rs` | Add `Jira` subcommand with `status`/`sync`/`update`, `--jira-sync` on `Process`/`Watch`, `JiraConfig` in `HearkenConfig` |
| (inline `#[cfg(test)]` modules) | Unit tests in each source file for marker parsing, filtering, base64, etc. |

---

### Task 1: Scaffold `hearken-jira` crate with config types

**Files:**
- Create: `hearken-jira/Cargo.toml`
- Create: `hearken-jira/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Write test for config validation**

Create `hearken-jira/src/lib.rs` with the config types and a test:

```rust
use serde::Deserialize;
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

    #[test]
    fn test_jira_config_from_env() {
        std::env::set_var("HEARKEN_JIRA_USER", "test@example.com");
        std::env::set_var("HEARKEN_JIRA_TOKEN", "secret-token");
        let toml = JiraTomlConfig {
            url: "https://myco.atlassian.net/".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let config = JiraConfig::from_toml_and_env(toml).unwrap();
        assert_eq!(config.url, "https://myco.atlassian.net"); // trailing slash stripped
        assert_eq!(config.issue_type, "Bug"); // default
        assert_eq!(config.user, "test@example.com");
        assert_eq!(config.token, "secret-token");
        std::env::remove_var("HEARKEN_JIRA_USER");
        std::env::remove_var("HEARKEN_JIRA_TOKEN");
    }

    #[test]
    fn test_jira_config_missing_env() {
        std::env::remove_var("HEARKEN_JIRA_USER");
        std::env::remove_var("HEARKEN_JIRA_TOKEN");
        let toml = JiraTomlConfig {
            url: "https://myco.atlassian.net".to_string(),
            project: "OPS".to_string(),
            label: "hearken".to_string(),
            instance_type: JiraInstanceType::Cloud,
            issue_type: None,
        };
        let err = JiraConfig::from_toml_and_env(toml).unwrap_err();
        assert!(err.to_string().contains("HEARKEN_JIRA_USER"));
    }
}
```

- [ ] **Step 2: Create `hearken-jira/Cargo.toml`**

```toml
[package]
name = "hearken-jira"
version = "0.2.0"
edition = "2024"

[dependencies]
hearken-core = { version = "0.2.0", path = "../hearken-core" }
hearken-storage = { version = "0.2.0", path = "../hearken-storage" }
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2.0.18"
toml = "0.8"
tokio = { version = "1", features = ["rt"] }

[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 3: Add `hearken-jira` to workspace**

In the root `Cargo.toml`, add `"hearken-jira"` to the workspace members:

```toml
[workspace]
members = [
    "hearken-cli",
    "hearken-core",
    "hearken-jira",
    "hearken-ml",
    "hearken-storage",
]
resolver = "2"
```

- [ ] **Step 4: Run tests to verify**

Run: `cd /Users/koraytaylan/Workspace/hearken && cargo test -p hearken-jira`
Expected: All 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add hearken-jira/ Cargo.toml
git commit -m "feat(jira): scaffold hearken-jira crate with config types"
```

---

### Task 2: Implement `mapper.rs` — ticket body generation and marker parsing

**Files:**
- Create: `hearken-jira/src/mapper.rs`
- Modify: `hearken-jira/src/lib.rs` (add `pub mod mapper;`)

- [ ] **Step 1: Write failing tests for marker parsing**

Create `hearken-jira/src/mapper.rs` with the marker data type and tests first (implementation stubs that panic):

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct HearkenMarker {
    pub db: String,
    pub pattern_id: i64,
    pub occurrences: i64,
}

/// Generates the code-block marker string for embedding in a JIRA ticket description.
pub fn build_marker(db: &str, pattern_id: i64, occurrences: i64) -> String {
    todo!()
}

/// Parses a hearken marker from a JIRA ticket description string.
/// Returns None if no valid marker is found.
pub fn parse_marker(description: &str) -> Option<HearkenMarker> {
    todo!()
}

/// Truncates a pattern template to fit JIRA's 255-char summary limit.
/// Prefixes with `[hearken] `.
pub fn build_summary(template: &str) -> String {
    todo!()
}

/// Data needed to generate a ticket body.
pub struct TicketBodyInput {
    pub template: String,
    pub occurrence_count: i64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub file_group: String,
    pub samples: Vec<String>,
    pub db_name: String,
    pub pattern_id: i64,
}

/// Generates a JIRA ticket description in wiki markup (for Server API v2).
pub fn build_description_wiki(input: &TicketBodyInput) -> String {
    todo!()
}

/// Generates a JIRA ticket description in ADF JSON (for Cloud API v3).
pub fn build_description_adf(input: &TicketBodyInput) -> serde_json::Value {
    todo!()
}

/// Generates a wiki-markup change comment.
pub fn build_change_comment_wiki(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: Option<&str>,
) -> String {
    todo!()
}

/// Generates an ADF change comment.
pub fn build_change_comment_adf(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: Option<&str>,
) -> serde_json::Value {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_and_parse_marker() {
        let marker = build_marker("myproject.db", 42, 1892);
        let parsed = parse_marker(&marker).unwrap();
        assert_eq!(parsed, HearkenMarker {
            db: "myproject.db".to_string(),
            pattern_id: 42,
            occurrences: 1892,
        });
    }

    #[test]
    fn test_parse_marker_from_full_description() {
        let desc = "Some ticket description content.\n\nMore details.\n\n{code:title=hearken-metadata}\nhearken:db=test.db;pattern_id=7;occurrences=500\n{code}";
        let parsed = parse_marker(desc).unwrap();
        assert_eq!(parsed.db, "test.db");
        assert_eq!(parsed.pattern_id, 7);
        assert_eq!(parsed.occurrences, 500);
    }

    #[test]
    fn test_parse_marker_no_marker() {
        assert!(parse_marker("Just a regular description").is_none());
        assert!(parse_marker("").is_none());
    }

    #[test]
    fn test_build_summary_short_template() {
        let summary = build_summary("ERROR user login failed");
        assert_eq!(summary, "[hearken] ERROR user login failed");
    }

    #[test]
    fn test_build_summary_long_template() {
        let long_template = "A".repeat(300);
        let summary = build_summary(&long_template);
        assert!(summary.len() <= 255);
        assert!(summary.starts_with("[hearken] "));
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn test_build_description_wiki_contains_marker() {
        let input = TicketBodyInput {
            template: "ERROR [pool-<*>] Service - failed in <*>ms".to_string(),
            occurrence_count: 1500,
            first_seen: Some("2026-01-15 08:00:00".to_string()),
            last_seen: Some("2026-04-05 14:30:00".to_string()),
            file_group: "error.log".to_string(),
            samples: vec!["ERROR [pool-3] Service - failed in 42ms".to_string()],
            db_name: "test.db".to_string(),
            pattern_id: 10,
        };
        let wiki = build_description_wiki(&input);
        assert!(wiki.contains("hearken:db=test.db;pattern_id=10;occurrences=1500"));
        assert!(wiki.contains("ERROR [pool-<*>] Service - failed in <*>ms"));
        assert!(wiki.contains("1500"));
        assert!(wiki.contains("error.log"));
    }

    #[test]
    fn test_build_description_adf_contains_marker() {
        let input = TicketBodyInput {
            template: "ERROR [pool-<*>] Service - failed".to_string(),
            occurrence_count: 200,
            first_seen: None,
            last_seen: None,
            file_group: "app.log".to_string(),
            samples: vec![],
            db_name: "mydb.db".to_string(),
            pattern_id: 5,
        };
        let adf = build_description_adf(&input);
        let adf_str = serde_json::to_string(&adf).unwrap();
        assert!(adf_str.contains("hearken:db=mydb.db;pattern_id=5;occurrences=200"));
    }

    #[test]
    fn test_change_comment_wiki() {
        let comment = build_change_comment_wiki(1000, 1500, Some("2026-04-05T14:30:00Z"));
        assert!(comment.contains("1,000"));
        assert!(comment.contains("1,500"));
        assert!(comment.contains("+500"));
        assert!(comment.contains("2026-04-05T14:30:00Z"));
    }

    #[test]
    fn test_change_comment_adf() {
        let adf = build_change_comment_adf(100, 250, Some("2026-04-05T14:30:00Z"));
        let adf_str = serde_json::to_string(&adf).unwrap();
        assert!(adf_str.contains("250"));
        assert!(adf_str.contains("+150"));
    }
}
```

- [ ] **Step 2: Add `pub mod mapper;` to `lib.rs`**

Add at the top of `hearken-jira/src/lib.rs`:

```rust
pub mod mapper;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p hearken-jira`
Expected: FAIL — all mapper tests hit `todo!()` panics.

- [ ] **Step 4: Implement marker functions**

In `mapper.rs`, replace the `todo!()` stubs for `build_marker` and `parse_marker`:

```rust
pub fn build_marker(db: &str, pattern_id: i64, occurrences: i64) -> String {
    format!(
        "{{code:title=hearken-metadata}}\nhearken:db={};pattern_id={};occurrences={}\n{{code}}",
        db, pattern_id, occurrences
    )
}

pub fn parse_marker(description: &str) -> Option<HearkenMarker> {
    // Find the marker line between {code} blocks
    for line in description.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("hearken:") {
            let mut db = None;
            let mut pattern_id = None;
            let mut occurrences = None;
            for part in rest.split(';') {
                if let Some((key, value)) = part.split_once('=') {
                    match key {
                        "db" => db = Some(value.to_string()),
                        "pattern_id" => pattern_id = value.parse().ok(),
                        "occurrences" => occurrences = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            if let (Some(db), Some(pid), Some(occ)) = (db, pattern_id, occurrences) {
                return Some(HearkenMarker {
                    db,
                    pattern_id: pid,
                    occurrences: occ,
                });
            }
        }
    }
    None
}
```

- [ ] **Step 5: Implement `build_summary`**

```rust
pub fn build_summary(template: &str) -> String {
    let prefix = "[hearken] ";
    let max_len = 255;
    let available = max_len - prefix.len();
    if template.len() <= available {
        format!("{}{}", prefix, template)
    } else {
        format!("{}{}...", prefix, &template[..available - 3])
    }
}
```

- [ ] **Step 6: Implement wiki markup description**

```rust
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut result = String::new();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

pub fn build_description_wiki(input: &TicketBodyInput) -> String {
    let mut desc = String::new();
    desc.push_str(&format!("h3. Pattern\n{{noformat}}\n{}\n{{noformat}}\n\n", input.template));
    desc.push_str(&format!("*Occurrences:* {}\n", format_number(input.occurrence_count)));
    if let Some(ref first) = input.first_seen {
        desc.push_str(&format!("*First seen:* {}\n", first));
    }
    if let Some(ref last) = input.last_seen {
        desc.push_str(&format!("*Last seen:* {}\n", last));
    }
    desc.push_str(&format!("*File group:* {}\n", input.file_group));

    if !input.samples.is_empty() {
        desc.push_str("\nh3. Sample Log Lines\n{noformat}\n");
        for sample in &input.samples {
            desc.push_str(sample);
            desc.push('\n');
        }
        desc.push_str("{noformat}\n");
    }

    desc.push('\n');
    desc.push_str(&build_marker(&input.db_name, input.pattern_id, input.occurrence_count));
    desc.push('\n');
    desc
}
```

- [ ] **Step 7: Implement ADF description**

```rust
pub fn build_description_adf(input: &TicketBodyInput) -> serde_json::Value {
    let mut content = vec![];

    // Heading: Pattern
    content.push(serde_json::json!({
        "type": "heading",
        "attrs": { "level": 3 },
        "content": [{ "type": "text", "text": "Pattern" }]
    }));

    // Code block with template
    content.push(serde_json::json!({
        "type": "codeBlock",
        "content": [{ "type": "text", "text": &input.template }]
    }));

    // Stats paragraph
    let mut stats_text = format!("Occurrences: {}", format_number(input.occurrence_count));
    if let Some(ref first) = input.first_seen {
        stats_text.push_str(&format!("\nFirst seen: {}", first));
    }
    if let Some(ref last) = input.last_seen {
        stats_text.push_str(&format!("\nLast seen: {}", last));
    }
    stats_text.push_str(&format!("\nFile group: {}", input.file_group));

    content.push(serde_json::json!({
        "type": "paragraph",
        "content": [{ "type": "text", "text": stats_text }]
    }));

    // Samples
    if !input.samples.is_empty() {
        content.push(serde_json::json!({
            "type": "heading",
            "attrs": { "level": 3 },
            "content": [{ "type": "text", "text": "Sample Log Lines" }]
        }));
        let samples_text = input.samples.join("\n");
        content.push(serde_json::json!({
            "type": "codeBlock",
            "content": [{ "type": "text", "text": samples_text }]
        }));
    }

    // Marker as code block
    let marker_line = format!(
        "hearken:db={};pattern_id={};occurrences={}",
        input.db_name, input.pattern_id, input.occurrence_count
    );
    content.push(serde_json::json!({
        "type": "codeBlock",
        "attrs": { "language": "hearken-metadata" },
        "content": [{ "type": "text", "text": marker_line }]
    }));

    serde_json::json!({
        "version": 1,
        "type": "doc",
        "content": content
    })
}
```

- [ ] **Step 8: Implement change comment functions**

```rust
pub fn build_change_comment_wiki(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: Option<&str>,
) -> String {
    let diff = new_occurrences - old_occurrences;
    let sign = if diff >= 0 { "+" } else { "" };
    let mut comment = format!(
        "[hearken sync] Updated {}\n* Occurrences: {} -> {} ({}{})\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        format_number(old_occurrences),
        format_number(new_occurrences),
        sign,
        format_number(diff),
    );
    if let Some(last) = last_seen {
        comment.push_str(&format!("* Last seen: {}\n", last));
    }
    comment
}

pub fn build_change_comment_adf(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: Option<&str>,
) -> serde_json::Value {
    let diff = new_occurrences - old_occurrences;
    let sign = if diff >= 0 { "+" } else { "" };
    let mut text = format!(
        "[hearken sync] Updated {}\nOccurrences: {} -> {} ({}{})",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        format_number(old_occurrences),
        format_number(new_occurrences),
        sign,
        format_number(diff),
    );
    if let Some(last) = last_seen {
        text.push_str(&format!("\nLast seen: {}", last));
    }

    serde_json::json!({
        "version": 1,
        "type": "doc",
        "content": [{
            "type": "paragraph",
            "content": [{ "type": "text", "text": text }]
        }]
    })
}
```

- [ ] **Step 9: Add chrono dependency to hearken-jira/Cargo.toml**

Add to `[dependencies]`:

```toml
chrono = { version = "0.4.44", features = ["serde"] }
```

- [ ] **Step 10: Run tests to verify**

Run: `cargo test -p hearken-jira`
Expected: All mapper tests pass. Some tests that check exact timestamp strings may need the assertions adjusted to use `contains()` for the date parts since `Utc::now()` varies.

- [ ] **Step 11: Commit**

```bash
git add hearken-jira/
git commit -m "feat(jira): implement ticket body generation and marker parsing"
```

---

### Task 3: Implement `filter.rs` — pattern filtering

**Files:**
- Create: `hearken-jira/src/filter.rs`
- Modify: `hearken-jira/src/lib.rs` (add `pub mod filter;`)

- [ ] **Step 1: Write failing tests for filter logic**

Create `hearken-jira/src/filter.rs`:

```rust
use hearken_storage::Storage;
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct FilterOptions {
    pub anomalies_only: bool,
    pub tags: Option<Vec<String>>,
    pub exclude_tags: Option<Vec<String>>,
    pub min_occurrences: Option<i64>,
    pub new_only: bool,
}

/// A pattern with its metadata, ready for JIRA sync.
#[derive(Debug, Clone)]
pub struct FilteredPattern {
    pub id: i64,
    pub template: String,
    pub occurrence_count: i64,
    pub file_group: String,
}

/// Loads all patterns from storage and applies the given filters.
/// `synced_pattern_ids` is the set of pattern IDs that already have JIRA tickets.
/// `anomaly_ids` is an optional pre-computed set of anomalous pattern IDs
/// (computed by the CLI using its private `compute_anomalies()` function).
pub fn filter_patterns(
    storage: &Storage,
    options: &FilterOptions,
    synced_pattern_ids: &HashSet<i64>,
    anomaly_ids: Option<&HashSet<i64>>,
) -> Result<Vec<FilteredPattern>, hearken_storage::StorageError> {
    // Get all patterns ranked (id, template, count, group)
    let all_patterns = storage.get_all_patterns_ranked(usize::MAX, None, None)?;

    let all_tags = if options.tags.is_some() || options.exclude_tags.is_some() {
        storage.get_all_tags()?
    } else {
        std::collections::HashMap::new()
    };

    let mut results: Vec<FilteredPattern> = Vec::new();

    for (id, template, count, group) in all_patterns {
        // anomalies_only filter: skip non-anomalous patterns
        if options.anomalies_only {
            if let Some(aids) = anomaly_ids {
                if !aids.contains(&id) {
                    continue;
                }
            }
        }

        // min_occurrences filter
        if let Some(min) = options.min_occurrences {
            if count < min {
                continue;
            }
        }

        // tags filter: pattern must have at least one of the specified tags
        if let Some(ref required_tags) = options.tags {
            let pattern_tags = all_tags.get(&id).cloned().unwrap_or_default();
            if !required_tags.iter().any(|t| pattern_tags.contains(t)) {
                continue;
            }
        }

        // exclude_tags filter: skip patterns that have any of the excluded tags
        if let Some(ref excluded) = options.exclude_tags {
            let pattern_tags = all_tags.get(&id).cloned().unwrap_or_default();
            if excluded.iter().any(|t| pattern_tags.contains(t)) {
                continue;
            }
        }

        // new_only filter: skip patterns already synced to JIRA
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
    use tempfile::tempdir;

    fn setup_db() -> (Storage, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let storage = Storage::open(db_path.to_str().unwrap()).unwrap();

        let group_id = storage.get_or_create_file_group("app.log").unwrap();

        // Insert patterns with different counts
        for (template, count) in [
            ("ERROR user <*> login failed", 100),
            ("WARN timeout after <*>ms", 50),
            ("INFO request completed in <*>ms", 500),
            ("ERROR database connection lost", 10),
        ] {
            storage.conn.execute(
                "INSERT INTO patterns (file_group_id, template, occurrence_count) VALUES (?, ?, ?)",
                rusqlite::params![group_id, template, count],
            ).unwrap();
        }

        // Tag pattern 1 with "critical"
        storage.add_tag(1, "critical").unwrap();
        // Tag pattern 4 with "suppressed"
        storage.add_tag(4, "suppressed").unwrap();

        (storage, dir)
    }

    #[test]
    fn test_filter_no_filters() {
        let (storage, _dir) = setup_db();
        let results = filter_patterns(&storage, &FilterOptions::default(), &HashSet::new(), None).unwrap();
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_filter_min_occurrences() {
        let (storage, _dir) = setup_db();
        let opts = FilterOptions {
            min_occurrences: Some(50),
            ..Default::default()
        };
        let results = filter_patterns(&storage, &opts, &HashSet::new(), None).unwrap();
        assert_eq!(results.len(), 3); // 100, 50, 500
    }

    #[test]
    fn test_filter_by_tags() {
        let (storage, _dir) = setup_db();
        let opts = FilterOptions {
            tags: Some(vec!["critical".to_string()]),
            ..Default::default()
        };
        let results = filter_patterns(&storage, &opts, &HashSet::new(), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].template.contains("login failed"));
    }

    #[test]
    fn test_filter_exclude_tags() {
        let (storage, _dir) = setup_db();
        let opts = FilterOptions {
            exclude_tags: Some(vec!["suppressed".to_string()]),
            ..Default::default()
        };
        let results = filter_patterns(&storage, &opts, &HashSet::new(), None).unwrap();
        assert_eq!(results.len(), 3); // pattern 4 excluded
    }

    #[test]
    fn test_filter_new_only() {
        let (storage, _dir) = setup_db();
        let mut synced = HashSet::new();
        synced.insert(1);
        synced.insert(3);
        let opts = FilterOptions {
            new_only: true,
            ..Default::default()
        };
        let results = filter_patterns(&storage, &opts, &synced, None).unwrap();
        assert_eq!(results.len(), 2); // patterns 2 and 4
    }

    #[test]
    fn test_filter_combined() {
        let (storage, _dir) = setup_db();
        let opts = FilterOptions {
            min_occurrences: Some(50),
            exclude_tags: Some(vec!["suppressed".to_string()]),
            ..Default::default()
        };
        let results = filter_patterns(&storage, &opts, &HashSet::new(), None).unwrap();
        assert_eq!(results.len(), 3);
    }
}
```

- [ ] **Step 2: Add `pub mod filter;` to `lib.rs`**

Add to `hearken-jira/src/lib.rs`:

```rust
pub mod filter;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p hearken-jira`
Expected: All tests pass (filter functions are already fully implemented above, not stubs).

- [ ] **Step 4: Commit**

```bash
git add hearken-jira/
git commit -m "feat(jira): implement pattern filtering with tags, thresholds, and new-only"
```

---

### Task 4: Implement `client.rs` — JIRA HTTP client

**Files:**
- Create: `hearken-jira/src/client.rs`
- Modify: `hearken-jira/src/lib.rs` (add `pub mod client;`)

- [ ] **Step 1: Create the JIRA client module**

Create `hearken-jira/src/client.rs`. This module makes real HTTP calls, so we test it via integration tests against a mock server. For now, we build the struct and its methods:

```rust
use crate::{JiraConfig, JiraError, JiraInstanceType};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
struct SearchResponse {
    issues: Vec<JiraIssue>,
    total: i64,
    #[serde(rename = "startAt")]
    start_at: i64,
    #[serde(rename = "maxResults")]
    max_results: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraIssue {
    pub key: String,
    pub fields: JiraIssueFields,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JiraIssueFields {
    pub summary: Option<String>,
    pub description: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CreateIssueResponse {
    key: String,
}

pub struct JiraClient {
    config: JiraConfig,
    http: reqwest::Client,
    api_base: String,
}

impl JiraClient {
    pub fn new(config: JiraConfig) -> Result<Self, JiraError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let auth_value = match config.instance_type {
            JiraInstanceType::Cloud => {
                let encoded =
                    base64_encode(&format!("{}:{}", config.user, config.token));
                format!("Basic {}", encoded)
            }
            JiraInstanceType::Server => {
                format!("Bearer {}", config.token)
            }
        };
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth_value)
                .map_err(|e| JiraError::Config(format!("Invalid auth header: {}", e)))?,
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        let api_version = match config.instance_type {
            JiraInstanceType::Cloud => "3",
            JiraInstanceType::Server => "2",
        };
        let api_base = format!("{}/rest/api/{}", config.url, api_version);

        Ok(Self {
            config,
            http,
            api_base,
        })
    }

    /// Search for all issues matching the given JQL, handling pagination.
    pub async fn search_issues(&self, jql: &str) -> Result<Vec<JiraIssue>, JiraError> {
        let mut all_issues = Vec::new();
        let mut start_at = 0i64;
        let max_results = 50;

        loop {
            let body = serde_json::json!({
                "jql": jql,
                "startAt": start_at,
                "maxResults": max_results,
                "fields": ["summary", "description"]
            });

            let resp = self
                .http
                .post(format!("{}/search", self.api_base))
                .json(&body)
                .send()
                .await?;

            if resp.status() == 429 {
                // Rate limited — respect Retry-After
                let retry_after = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5);
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            let status = resp.status();
            if !status.is_success() {
                let msg = resp.text().await.unwrap_or_default();
                return Err(JiraError::Api {
                    status: status.as_u16(),
                    message: msg,
                });
            }

            let search_resp: SearchResponse = resp.json().await?;
            let returned = search_resp.issues.len() as i64;
            all_issues.extend(search_resp.issues);

            if start_at + returned >= search_resp.total {
                break;
            }
            start_at += returned;
        }

        Ok(all_issues)
    }

    /// Fetch all hearken-managed tickets for the configured project and label.
    pub async fn fetch_hearken_tickets(&self) -> Result<Vec<JiraIssue>, JiraError> {
        let jql = format!(
            "project = \"{}\" AND labels = \"{}\"",
            self.config.project, self.config.label
        );
        self.search_issues(&jql).await
    }

    /// Create a new JIRA issue. Returns the issue key (e.g., "OPS-1234").
    pub async fn create_issue(
        &self,
        summary: &str,
        description: serde_json::Value,
        label: &str,
        issue_type: &str,
    ) -> Result<String, JiraError> {
        let body = match self.config.instance_type {
            JiraInstanceType::Cloud => serde_json::json!({
                "fields": {
                    "project": { "key": &self.config.project },
                    "summary": summary,
                    "description": description,
                    "issuetype": { "name": issue_type },
                    "labels": [label]
                }
            }),
            JiraInstanceType::Server => serde_json::json!({
                "fields": {
                    "project": { "key": &self.config.project },
                    "summary": summary,
                    "description": description,
                    "issuetype": { "name": issue_type },
                    "labels": [label]
                }
            }),
        };

        let resp = self
            .http
            .post(format!("{}/issue", self.api_base))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message: msg,
            });
        }

        let created: CreateIssueResponse = resp.json().await?;
        Ok(created.key)
    }

    /// Update the description of an existing issue.
    pub async fn update_issue_description(
        &self,
        issue_key: &str,
        description: serde_json::Value,
    ) -> Result<(), JiraError> {
        let body = serde_json::json!({
            "fields": {
                "description": description
            }
        });

        let resp = self
            .http
            .put(format!("{}/issue/{}", self.api_base, issue_key))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message: msg,
            });
        }
        Ok(())
    }

    /// Add a comment to an existing issue.
    pub async fn add_comment(
        &self,
        issue_key: &str,
        body_content: serde_json::Value,
    ) -> Result<(), JiraError> {
        let body = match self.config.instance_type {
            JiraInstanceType::Cloud => serde_json::json!({ "body": body_content }),
            JiraInstanceType::Server => serde_json::json!({ "body": body_content }),
        };

        let resp = self
            .http
            .post(format!("{}/issue/{}/comment", self.api_base, issue_key))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(JiraError::Api {
                status: status.as_u16(),
                message: msg,
            });
        }
        Ok(())
    }

    pub fn config(&self) -> &JiraConfig {
        &self.config
    }
}

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode("user:token"), "dXNlcjp0b2tlbg==");
        assert_eq!(base64_encode("test@example.com:abc123"), "dGVzdEBleGFtcGxlLmNvbTphYmMxMjM=");
    }
}
```

- [ ] **Step 2: Add `pub mod client;` to `lib.rs`**

Add to `hearken-jira/src/lib.rs`:

```rust
pub mod client;
```

- [ ] **Step 3: Run tests to verify**

Run: `cargo test -p hearken-jira`
Expected: All tests pass (base64 test + previous tests).

- [ ] **Step 4: Commit**

```bash
git add hearken-jira/
git commit -m "feat(jira): implement JIRA HTTP client with Cloud/Server auth and pagination"
```

---

### Task 5: Implement orchestration in `lib.rs` — `sync()`, `update()`, `status()`

**Files:**
- Modify: `hearken-jira/src/lib.rs`

- [ ] **Step 1: Add orchestration types and functions**

Add the following to `hearken-jira/src/lib.rs`, below the existing config code and above the tests module:

```rust
pub mod client;
pub mod filter;
pub mod mapper;

use client::{JiraClient, JiraIssue};
use filter::{FilterOptions, FilteredPattern, filter_patterns};
use mapper::{HearkenMarker, parse_marker};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct SyncResult {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: usize,
    pub failed: Vec<(String, String)>, // (pattern_template_preview, error_message)
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
            let preview = if tmpl.len() > 60 { &tmpl[..60] } else { tmpl };
            eprintln!("  ! Failed: {} — {}", preview, err);
        }
    }
}

/// Extract description text from a JIRA issue, handling both ADF (Cloud) and plain text (Server).
fn extract_description_text(issue: &JiraIssue) -> String {
    match &issue.fields.description {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(val) => {
            // ADF format — walk the content tree to find text nodes
            extract_adf_text(val)
        }
        None => String::new(),
    }
}

fn extract_adf_text(node: &serde_json::Value) -> String {
    let mut text = String::new();
    if let Some(t) = node.get("text").and_then(|v| v.as_str()) {
        text.push_str(t);
        text.push('\n');
    }
    if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
        for child in content {
            text.push_str(&extract_adf_text(child));
        }
    }
    text
}

/// Build the mapping from (db, pattern_id) to JIRA issue, by fetching all
/// hearken-labelled tickets and parsing their markers.
async fn build_ticket_map(
    client: &JiraClient,
) -> Result<HashMap<(String, i64), (JiraIssue, HearkenMarker)>, JiraError> {
    let issues = client.fetch_hearken_tickets().await?;
    let mut map = HashMap::new();
    for issue in issues {
        let desc_text = extract_description_text(&issue);
        if let Some(marker) = parse_marker(&desc_text) {
            let key = (marker.db.clone(), marker.pattern_id);
            map.insert(key, (issue, marker));
        }
    }
    Ok(map)
}

/// Get first/last seen timestamps for a pattern from the occurrences table.
fn get_pattern_timestamps(
    storage: &hearken_storage::Storage,
    pattern_id: i64,
) -> (Option<String>, Option<String>) {
    let first: Option<String> = storage
        .conn
        .query_row(
            "SELECT MIN(entry_timestamp) FROM occurrences WHERE pattern_id = ? AND entry_timestamp IS NOT NULL",
            rusqlite::params![pattern_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| ts.to_string())
        });

    let last: Option<String> = storage
        .conn
        .query_row(
            "SELECT MAX(entry_timestamp) FROM occurrences WHERE pattern_id = ? AND entry_timestamp IS NOT NULL",
            rusqlite::params![pattern_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| ts.to_string())
        });

    (first, last)
}

/// Get sample log lines for a pattern (variable values reconstructed into template).
fn get_sample_lines(
    storage: &hearken_storage::Storage,
    pattern_id: i64,
    limit: usize,
) -> Vec<String> {
    storage
        .get_pattern_samples(pattern_id, limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(vars, _file)| vars.replace('\t', " | "))
        .collect()
}

/// Run a full sync: create new tickets + update existing ones.
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

    let synced_ids: HashSet<i64> = ticket_map
        .iter()
        .filter(|((db, _), _)| db == db_name)
        .map(|((_, pid), _)| *pid)
        .collect();

    let patterns = filter_patterns(storage, filter_opts, &synced_ids, anomaly_ids)?;
    let mut result = SyncResult::default();

    for pattern in &patterns {
        let key = (db_name.to_string(), pattern.id);

        if let Some((issue, marker)) = ticket_map.get(&key) {
            // Existing ticket — update if changed
            if marker.occurrences == pattern.occurrence_count {
                result.unchanged += 1;
                continue;
            }

            if dry_run {
                let preview = if pattern.template.len() > 60 {
                    format!("{}...", &pattern.template[..60])
                } else {
                    pattern.template.clone()
                };
                println!("  [dry-run] Would update {}: {}", issue.key, preview);
                result.updated.push(issue.key.clone());
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

            let (desc, comment) = match config.instance_type {
                JiraInstanceType::Cloud => (
                    mapper::build_description_adf(&input),
                    mapper::build_change_comment_adf(
                        marker.occurrences,
                        pattern.occurrence_count,
                        last_seen.as_deref(),
                    ),
                ),
                JiraInstanceType::Server => (
                    serde_json::Value::String(mapper::build_description_wiki(&input)),
                    serde_json::Value::String(mapper::build_change_comment_wiki(
                        marker.occurrences,
                        pattern.occurrence_count,
                        last_seen.as_deref(),
                    )),
                ),
            };

            match client.update_issue_description(&issue.key, desc).await {
                Ok(()) => {
                    let _ = client.add_comment(&issue.key, comment).await;
                    result.updated.push(issue.key.clone());
                }
                Err(e) => {
                    let preview = if pattern.template.len() > 60 {
                        format!("{}...", &pattern.template[..60])
                    } else {
                        pattern.template.clone()
                    };
                    result.failed.push((preview, e.to_string()));
                }
            }
        } else {
            // New ticket — create
            if dry_run {
                let preview = if pattern.template.len() > 60 {
                    format!("{}...", &pattern.template[..60])
                } else {
                    pattern.template.clone()
                };
                println!("  [dry-run] Would create: {}", preview);
                result.created.push("DRY-RUN".to_string());
                continue;
            }

            let (first_seen, last_seen) = get_pattern_timestamps(storage, pattern.id);
            let samples = get_sample_lines(storage, pattern.id, 5);
            let summary = mapper::build_summary(&pattern.template);

            let input = mapper::TicketBodyInput {
                template: pattern.template.clone(),
                occurrence_count: pattern.occurrence_count,
                first_seen,
                last_seen,
                file_group: pattern.file_group.clone(),
                samples,
                db_name: db_name.to_string(),
                pattern_id: pattern.id,
            };

            let desc = match config.instance_type {
                JiraInstanceType::Cloud => mapper::build_description_adf(&input),
                JiraInstanceType::Server => {
                    serde_json::Value::String(mapper::build_description_wiki(&input))
                }
            };

            match client
                .create_issue(&summary, desc, &config.label, &config.issue_type)
                .await
            {
                Ok(key) => result.created.push(key),
                Err(e) => {
                    let preview = if pattern.template.len() > 60 {
                        format!("{}...", &pattern.template[..60])
                    } else {
                        pattern.template.clone()
                    };
                    result.failed.push((preview, e.to_string()));
                }
            }
        }
    }

    Ok(result)
}

/// Update only existing tickets (no new ticket creation).
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
        .iter()
        .filter(|((db, _), _)| db == db_name)
        .map(|((_, pid), _)| *pid)
        .collect();

    // Only process patterns that already have tickets
    let mut filter_for_existing = filter_opts.clone();
    filter_for_existing.new_only = false;

    let patterns = filter_patterns(storage, &filter_for_existing, &synced_ids, anomaly_ids)?;
    let mut result = SyncResult::default();

    for pattern in &patterns {
        let key = (db_name.to_string(), pattern.id);
        if let Some((issue, marker)) = ticket_map.get(&key) {
            if marker.occurrences == pattern.occurrence_count {
                result.unchanged += 1;
                continue;
            }

            if dry_run {
                println!("  [dry-run] Would update {}", issue.key);
                result.updated.push(issue.key.clone());
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

            let (desc, comment) = match config.instance_type {
                JiraInstanceType::Cloud => (
                    mapper::build_description_adf(&input),
                    mapper::build_change_comment_adf(
                        marker.occurrences,
                        pattern.occurrence_count,
                        last_seen.as_deref(),
                    ),
                ),
                JiraInstanceType::Server => (
                    serde_json::Value::String(mapper::build_description_wiki(&input)),
                    serde_json::Value::String(mapper::build_change_comment_wiki(
                        marker.occurrences,
                        pattern.occurrence_count,
                        last_seen.as_deref(),
                    )),
                ),
            };

            match client.update_issue_description(&issue.key, desc).await {
                Ok(()) => {
                    let _ = client.add_comment(&issue.key, comment).await;
                    result.updated.push(issue.key.clone());
                }
                Err(e) => {
                    let preview = if pattern.template.len() > 60 {
                        format!("{}...", &pattern.template[..60])
                    } else {
                        pattern.template.clone()
                    };
                    result.failed.push((preview, e.to_string()));
                }
            }
        }
        // Skip patterns without tickets — this is `update`, not `sync`
    }

    Ok(result)
}

/// Show sync status without making any changes.
pub async fn status(
    config: JiraConfig,
    storage: &hearken_storage::Storage,
    db_name: &str,
) -> Result<(), JiraError> {
    let client = JiraClient::new(config.clone())?;
    let ticket_map = build_ticket_map(&client).await?;

    let all_patterns = storage.get_all_patterns_ranked(usize::MAX, None, None)?;
    let total_patterns = all_patterns.len();

    let mut with_tickets = 0usize;
    let mut changed = 0usize;

    let pattern_counts: HashMap<i64, i64> = all_patterns
        .iter()
        .map(|(id, _, count, _)| (*id, *count))
        .collect();

    for ((db, pid), (_issue, marker)) in &ticket_map {
        if db == db_name {
            with_tickets += 1;
            if let Some(&current_count) = pattern_counts.get(pid) {
                if current_count != marker.occurrences {
                    changed += 1;
                }
            }
        }
    }

    let new = total_patterns.saturating_sub(with_tickets);

    println!(
        "JIRA Sync Status (project: {}, label: {})",
        config.project, config.label
    );
    println!("  Total patterns:     {}", total_patterns);
    println!("  With JIRA tickets:  {}", with_tickets);
    println!("  New (unsynced):     {}", new);
    println!("  Changed since sync: {}", changed);
    println!("  JIRA connection:    OK");

    Ok(())
}
```

- [ ] **Step 2: Run tests to verify compilation**

Run: `cargo test -p hearken-jira`
Expected: All existing tests still pass, no compilation errors.

- [ ] **Step 3: Commit**

```bash
git add hearken-jira/
git commit -m "feat(jira): implement sync, update, and status orchestration"
```

---

### Task 6: Wire JIRA into `hearken-cli` — feature flag, config, and subcommands

**Files:**
- Modify: `hearken-cli/Cargo.toml`
- Modify: `hearken-cli/src/main.rs`

- [ ] **Step 1: Add feature flag and dependency to hearken-cli/Cargo.toml**

In `hearken-cli/Cargo.toml`, add to `[features]`:

```toml
[features]
web = ["axum", "tokio", "tower-http"]
jira = ["hearken-jira", "tokio"]
```

And add to `[dependencies]`:

```toml
hearken-jira = { version = "0.2.0", path = "../hearken-jira", optional = true }
```

Note: `tokio` is already listed as an optional dep (for web). The `jira` feature also needs it for the async runtime. Since `tokio` is already in `[dependencies]` as optional, the `jira` feature just needs to enable it.

- [ ] **Step 2: Add `[jira]` section to `HearkenConfig`**

In `hearken-cli/src/main.rs`, add the JIRA config field to `HearkenConfig`:

```rust
#[derive(Deserialize, Default, Debug)]
struct HearkenConfig {
    database: Option<String>,
    threshold: Option<f64>,
    batch_size: Option<usize>,
    #[serde(default)]
    report: ReportConfig,
    #[serde(default)]
    export: ExportConfig,
    #[serde(default)]
    check: CheckConfig,
    #[cfg(feature = "jira")]
    #[serde(default)]
    jira: Option<hearken_jira::JiraTomlConfig>,
}
```

- [ ] **Step 3: Add `Jira` subcommand and `JiraAction` enum to `Commands`**

Add the `Jira` variant to the `Commands` enum and a new `JiraAction` subcommand enum. Also add `--jira-sync` flag to `Process` and `Watch`:

```rust
// Add to Commands enum, just before the #[cfg(feature = "web")] Serve variant:
    /// JIRA integration: create and manage JIRA tickets from patterns
    #[cfg(feature = "jira")]
    Jira {
        #[command(subcommand)]
        action: JiraAction,
    },
```

```rust
// Add new enum after BaselineAction:
#[cfg(feature = "jira")]
#[derive(Subcommand)]
enum JiraAction {
    /// Show sync status: pattern counts, ticket counts, connection check
    Status {},
    /// Create new tickets and update existing ones
    Sync {
        /// Only patterns flagged as anomalies
        #[arg(long)]
        anomalies_only: bool,
        /// Only patterns with these tags (comma-separated)
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
        /// Exclude patterns with these tags (comma-separated)
        #[arg(long, value_delimiter = ',')]
        exclude_tags: Option<Vec<String>>,
        /// Only patterns with at least this many occurrences
        #[arg(long)]
        min_occurrences: Option<i64>,
        /// Only patterns not yet synced to JIRA
        #[arg(long)]
        new_only: bool,
        /// Show what would happen without making API calls
        #[arg(long)]
        dry_run: bool,
    },
    /// Update existing JIRA tickets only (no new ticket creation)
    Update {
        /// Only patterns flagged as anomalies
        #[arg(long)]
        anomalies_only: bool,
        /// Only patterns with these tags (comma-separated)
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
        /// Exclude patterns with these tags (comma-separated)
        #[arg(long, value_delimiter = ',')]
        exclude_tags: Option<Vec<String>>,
        /// Only patterns with at least this many occurrences
        #[arg(long)]
        min_occurrences: Option<i64>,
        /// Show what would happen without making API calls
        #[arg(long)]
        dry_run: bool,
    },
}
```

Add `--jira-sync` to the `Process` command struct:

```rust
    Process {
        // ... existing fields ...
        /// After processing, sync patterns to JIRA
        #[cfg(feature = "jira")]
        #[arg(long)]
        jira_sync: bool,
    },
```

And to `Watch`:

```rust
    Watch {
        // ... existing fields ...
        /// After processing, sync patterns to JIRA
        #[cfg(feature = "jira")]
        #[arg(long)]
        jira_sync: bool,
    },
```

- [ ] **Step 4: Add match arm for `Jira` command**

In the `match cli.command` block, add before the `#[cfg(feature = "web")]` Serve arm:

```rust
        #[cfg(feature = "jira")]
        Commands::Jira { action } => {
            let jira_toml = config.jira.ok_or_else(|| {
                anyhow::anyhow!("No [jira] section found in .hearken.toml. See docs for configuration.")
            })?;
            let jira_config = hearken_jira::JiraConfig::from_toml_and_env(jira_toml)
                .context("Failed to load JIRA configuration")?;
            let db_name = Path::new(&cli.database)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(&cli.database)
                .to_string();

            let rt = tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")?;

            match action {
                JiraAction::Status {} => {
                    rt.block_on(hearken_jira::status(jira_config, &storage, &db_name))?;
                }
                JiraAction::Sync {
                    anomalies_only,
                    tags,
                    exclude_tags,
                    min_occurrences,
                    new_only,
                    dry_run,
                } => {
                    let filter_opts = hearken_jira::filter::FilterOptions {
                        anomalies_only,
                        tags,
                        exclude_tags,
                        min_occurrences,
                        new_only,
                    };
                    let anomaly_ids = if anomalies_only {
                        let anomalies = compute_anomalies(&storage, None, usize::MAX, &HashSet::new())?;
                        Some(anomalies.iter().map(|a| a.id).collect::<HashSet<i64>>())
                    } else {
                        None
                    };
                    let result = rt.block_on(hearken_jira::sync(
                        jira_config,
                        &storage,
                        &db_name,
                        &filter_opts,
                        anomaly_ids.as_ref(),
                        dry_run,
                    ))?;
                    result.print_summary();
                }
                JiraAction::Update {
                    anomalies_only,
                    tags,
                    exclude_tags,
                    min_occurrences,
                    dry_run,
                } => {
                    let filter_opts = hearken_jira::filter::FilterOptions {
                        anomalies_only,
                        tags,
                        exclude_tags,
                        min_occurrences,
                        new_only: false,
                    };
                    let anomaly_ids = if anomalies_only {
                        let anomalies = compute_anomalies(&storage, None, usize::MAX, &HashSet::new())?;
                        Some(anomalies.iter().map(|a| a.id).collect::<HashSet<i64>>())
                    } else {
                        None
                    };
                    let result = rt.block_on(hearken_jira::update(
                        jira_config,
                        &storage,
                        &db_name,
                        &filter_opts,
                        anomaly_ids.as_ref(),
                        dry_run,
                    ))?;
                    result.print_summary();
                }
            }
        }
```

- [ ] **Step 5: Add `--jira-sync` handling after process completes**

In the `Commands::Process` match arm, after the `process_files()` call (around line 396), add:

```rust
            #[cfg(feature = "jira")]
            if jira_sync {
                if let Some(jira_toml) = config.jira {
                    let jira_config = hearken_jira::JiraConfig::from_toml_and_env(jira_toml)
                        .context("Failed to load JIRA configuration")?;
                    let db_name = Path::new(&cli.database)
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(&cli.database)
                        .to_string();
                    let rt = tokio::runtime::Runtime::new()
                        .context("Failed to create Tokio runtime")?;
                    let result = rt.block_on(hearken_jira::sync(
                        jira_config,
                        &storage,
                        &db_name,
                        &hearken_jira::filter::FilterOptions::default(),
                        None,
                        false,
                    ))?;
                    result.print_summary();
                } else {
                    eprintln!("Warning: --jira-sync specified but no [jira] section in config");
                }
            }
```

Note: The `storage` variable was moved with `let mut storage = storage;` so we need to use `&storage` after `process_files` returns.

- [ ] **Step 5b: Add `--jira-sync` handling after watch batch completes**

In the `Commands::Watch` match arm, after the `watch_files()` call completes (the watch loop exits on ctrl-c), add the same jira-sync block as Process. Note: in practice watch runs indefinitely, so `--jira-sync` is most useful with `process`. For watch, it would sync on exit. The implementation is identical to the Process block above.

- [ ] **Step 5c: Compute anomaly IDs in CLI when `anomalies_only` is set**

In the `JiraAction::Sync` and `JiraAction::Update` match arms, before calling `hearken_jira::sync`/`update`, compute anomaly IDs if the flag is set. The CLI has access to `compute_anomalies()`. Add this logic:

```rust
                let anomaly_ids = if anomalies_only {
                    let anomalies = compute_anomalies(&storage, None, usize::MAX, &HashSet::new())?;
                    Some(anomalies.iter().map(|a| a.id).collect::<HashSet<i64>>())
                } else {
                    None
                };
```

Then pass `anomaly_ids.as_ref()` to `filter_patterns` through the sync/update functions. This means the `sync()` and `update()` functions in `lib.rs` need an additional `anomaly_ids: Option<&HashSet<i64>>` parameter, which they forward to `filter_patterns()`.

- [ ] **Step 6: Build with jira feature to verify compilation**

Run: `cargo build -p hearken-cli --features jira`
Expected: Compiles without errors.

- [ ] **Step 7: Build without jira feature to verify no impact**

Run: `cargo build -p hearken-cli`
Expected: Compiles without errors, no JIRA-related code included.

- [ ] **Step 8: Commit**

```bash
git add hearken-cli/Cargo.toml hearken-cli/src/main.rs
git commit -m "feat(jira): wire JIRA subcommands and --jira-sync into CLI"
```

---

### Task 7: Run full test suite and fix issues

**Files:**
- Any files that need fixes

- [ ] **Step 1: Run all tests without jira feature**

Run: `cargo test`
Expected: All existing tests pass — JIRA code is completely gated.

- [ ] **Step 2: Run all tests with jira feature**

Run: `cargo test --features jira`
Expected: All tests pass including hearken-jira crate tests.

- [ ] **Step 3: Run clippy with jira feature**

Run: `cargo clippy --features jira -- -D warnings`
Expected: No warnings.

- [ ] **Step 4: Run clippy without jira feature**

Run: `cargo clippy -- -D warnings`
Expected: No warnings.

- [ ] **Step 5: Fix any issues found in steps 1-4**

Address any compilation errors, test failures, or clippy warnings.

- [ ] **Step 6: Commit fixes if any**

```bash
git add -A
git commit -m "fix(jira): address test failures and clippy warnings"
```

---

### Task 8: Create PR

- [ ] **Step 1: Push branch and create PR**

```bash
git push -u origin feature/jira-integration
gh pr create --title "feat: JIRA integration for pattern-to-ticket sync" --body "$(cat <<'EOF'
## Summary
- New `hearken-jira` crate behind `jira` feature flag
- `hearken jira status/sync/update` subcommands for managing JIRA tickets from discovered patterns
- `--jira-sync` flag on `process` and `watch` for inline integration
- Supports both JIRA Cloud (API v3, ADF) and Server/Data Center (API v2, wiki markup)
- Rich filtering: anomalies, tags, thresholds, new-only, dry-run

## Design
See `docs/superpowers/specs/2026-04-05-jira-integration-design.md`

## Test plan
- [ ] Unit tests for config deserialization and validation
- [ ] Unit tests for marker generation and round-trip parsing
- [ ] Unit tests for pattern filtering (tags, thresholds, new-only)
- [ ] Build succeeds with `--features jira` and without
- [ ] Clippy clean with and without jira feature
- [ ] Integration test with real JIRA instance (manual)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
