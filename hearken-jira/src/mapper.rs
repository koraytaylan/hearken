use chrono::Utc;
use serde_json::{json, Value};

/// Metadata marker embedded in JIRA ticket descriptions so hearken can
/// identify and update its own tickets.
#[derive(Debug, Clone, PartialEq)]
pub struct HearkenMarker {
    pub db: String,
    pub pattern_id: i64,
    pub occurrences: i64,
}

/// All data needed to render a JIRA ticket body.
#[derive(Debug, Clone)]
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

// ---------------------------------------------------------------------------
// Marker helpers
// ---------------------------------------------------------------------------

/// Generates a wiki-markup code block containing the hearken metadata marker.
///
/// Format (wiki markup, not Rust code):
/// ```text
/// {code:title=hearken-metadata}
/// hearken:db=X;pattern_id=Y;occurrences=Z
/// {code}
/// ```
pub fn build_marker(db: &str, pattern_id: i64, occurrences: i64) -> String {
    format!(
        "{{code:title=hearken-metadata}}\nhearken:db={};pattern_id={};occurrences={}\n{{code}}",
        db, pattern_id, occurrences
    )
}

/// Scans `description` line-by-line for a `hearken:` marker and parses it.
/// Returns `None` if no valid marker is found.
pub fn parse_marker(description: &str) -> Option<HearkenMarker> {
    for line in description.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("hearken:") {
            let mut db: Option<String> = None;
            let mut pattern_id: Option<i64> = None;
            let mut occurrences: Option<i64> = None;

            for pair in rest.split(';') {
                let pair = pair.trim();
                if let Some((key, value)) = pair.split_once('=') {
                    match key.trim() {
                        "db" => db = Some(value.trim().to_string()),
                        "pattern_id" => pattern_id = value.trim().parse().ok(),
                        "occurrences" => occurrences = value.trim().parse().ok(),
                        _ => {}
                    }
                }
            }

            if let (Some(db), Some(pattern_id), Some(occurrences)) = (db, pattern_id, occurrences)
            {
                return Some(HearkenMarker {
                    db,
                    pattern_id,
                    occurrences,
                });
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

/// Prefixes the template with `[hearken] ` and truncates to 255 characters
/// (appending `...`) if necessary.
pub fn build_summary(template: &str) -> String {
    let prefix = "[hearken] ";
    let raw = format!("{}{}", prefix, template);
    if raw.len() > 255 {
        // Find a char-boundary-safe truncation point
        let mut end = 252;
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &raw[..end])
    } else {
        raw
    }
}

// ---------------------------------------------------------------------------
// Number formatting
// ---------------------------------------------------------------------------

/// Formats an integer with comma separators (e.g. `1_500_000` -> `"1,500,000"`).
pub fn format_number(n: i64) -> String {
    let s = n.abs().to_string();
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(*ch);
    }
    if n < 0 {
        format!("-{}", result)
    } else {
        result
    }
}

// ---------------------------------------------------------------------------
// Wiki markup body
// ---------------------------------------------------------------------------

/// Builds the full JIRA Server / Data Center wiki-markup ticket description.
pub fn build_description_wiki(input: &TicketBodyInput) -> String {
    let mut body = String::new();

    // Pattern section
    body.push_str("h3. Pattern\n");
    body.push_str("{noformat}\n");
    body.push_str(&input.template);
    body.push_str("\n{noformat}\n\n");

    // Stats
    body.push_str(&format!(
        "*Occurrences:* {}\n",
        format_number(input.occurrence_count)
    ));
    if let Some(ref first) = input.first_seen {
        body.push_str(&format!("*First seen:* {}\n", first));
    }
    if let Some(ref last) = input.last_seen {
        body.push_str(&format!("*Last seen:* {}\n", last));
    }
    body.push_str(&format!("*File group:* {}\n", input.file_group));

    // Sample log lines
    if !input.samples.is_empty() {
        body.push_str("\nh3. Sample Log Lines\n");
        body.push_str("{noformat}\n");
        body.push_str(&input.samples.join("\n"));
        body.push_str("\n{noformat}\n");
    }

    // Marker
    body.push('\n');
    body.push_str(&build_marker(&input.db_name, input.pattern_id, input.occurrence_count));
    body.push('\n');

    body
}

// ---------------------------------------------------------------------------
// ADF (Atlassian Document Format) body
// ---------------------------------------------------------------------------

/// Builds the full JIRA Cloud ADF JSON ticket description.
pub fn build_description_adf(input: &TicketBodyInput) -> Value {
    let mut content: Vec<Value> = Vec::new();

    // h3 heading "Pattern"
    content.push(heading_node(3, "Pattern"));

    // codeBlock with template
    content.push(code_block_node(None, &input.template));

    // Stats paragraph
    let mut stats = format!("Occurrences: {}", format_number(input.occurrence_count));
    if let Some(ref first) = input.first_seen {
        stats.push_str(&format!(" | First seen: {}", first));
    }
    if let Some(ref last) = input.last_seen {
        stats.push_str(&format!(" | Last seen: {}", last));
    }
    stats.push_str(&format!(" | File group: {}", input.file_group));
    content.push(paragraph_node(&stats));

    // Sample log lines
    if !input.samples.is_empty() {
        content.push(heading_node(3, "Sample Log Lines"));
        content.push(code_block_node(None, &input.samples.join("\n")));
    }

    // Marker codeBlock
    let marker_line = format!(
        "hearken:db={};pattern_id={};occurrences={}",
        input.db_name, input.pattern_id, input.occurrence_count
    );
    content.push(code_block_node(Some("hearken-metadata"), &marker_line));

    json!({
        "version": 1,
        "type": "doc",
        "content": content
    })
}

// ---------------------------------------------------------------------------
// Change comment — wiki markup
// ---------------------------------------------------------------------------

/// Builds a wiki-markup sync comment noting what changed.
pub fn build_change_comment_wiki(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: &str,
) -> String {
    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let diff = new_occurrences - old_occurrences;
    let sign = if diff >= 0 { "+" } else { "" };
    format!(
        "[hearken sync] Updated {}\n* Occurrences: {} -> {} ({}{})\n* Last seen: {}",
        timestamp,
        format_number(old_occurrences),
        format_number(new_occurrences),
        sign,
        format_number(diff),
        last_seen,
    )
}

// ---------------------------------------------------------------------------
// Change comment — ADF
// ---------------------------------------------------------------------------

/// Builds an ADF sync comment noting what changed.
pub fn build_change_comment_adf(
    old_occurrences: i64,
    new_occurrences: i64,
    last_seen: &str,
) -> Value {
    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let diff = new_occurrences - old_occurrences;
    let sign = if diff >= 0 { "+" } else { "" };
    let text = format!(
        "[hearken sync] Updated {} | Occurrences: {} -> {} ({}{}) | Last seen: {}",
        timestamp,
        format_number(old_occurrences),
        format_number(new_occurrences),
        sign,
        format_number(diff),
        last_seen,
    );
    json!({
        "version": 1,
        "type": "doc",
        "content": [paragraph_node(&text)]
    })
}

// ---------------------------------------------------------------------------
// ADF node builders (private helpers)
// ---------------------------------------------------------------------------

fn heading_node(level: u8, text: &str) -> Value {
    json!({
        "type": "heading",
        "attrs": { "level": level },
        "content": [{ "type": "text", "text": text }]
    })
}

fn paragraph_node(text: &str) -> Value {
    json!({
        "type": "paragraph",
        "content": [{ "type": "text", "text": text }]
    })
}

fn code_block_node(language: Option<&str>, code: &str) -> Value {
    let attrs = match language {
        Some(lang) => json!({ "language": lang }),
        None => json!({}),
    };
    json!({
        "type": "codeBlock",
        "attrs": attrs,
        "content": [{ "type": "text", "text": code }]
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_and_parse_marker() {
        let marker_str = build_marker("mydb", 42, 1500);
        assert!(marker_str.contains("hearken:db=mydb;pattern_id=42;occurrences=1500"));

        let parsed = parse_marker(&marker_str).expect("should parse marker");
        assert_eq!(
            parsed,
            HearkenMarker {
                db: "mydb".to_string(),
                pattern_id: 42,
                occurrences: 1500,
            }
        );
    }

    #[test]
    fn test_parse_marker_from_full_description() {
        let description = "\
h3. Pattern
{noformat}
ERROR something
{noformat}

*Occurrences:* 1,500

{code:title=hearken-metadata}
hearken:db=prod;pattern_id=7;occurrences=1500
{code}
";
        let parsed = parse_marker(description).expect("should find marker in full description");
        assert_eq!(parsed.db, "prod");
        assert_eq!(parsed.pattern_id, 7);
        assert_eq!(parsed.occurrences, 1500);
    }

    #[test]
    fn test_parse_marker_no_marker() {
        let text = "This is a regular description with no hearken marker inside.";
        assert_eq!(parse_marker(text), None);
    }

    #[test]
    fn test_build_summary_short_template() {
        let summary = build_summary("ERROR at startup");
        assert_eq!(summary, "[hearken] ERROR at startup");
    }

    #[test]
    fn test_build_summary_long_template() {
        // Build a template longer than 255 characters after the prefix is added.
        let long_template = "X".repeat(300);
        let summary = build_summary(&long_template);
        assert_eq!(summary.len(), 255);
        assert!(summary.starts_with("[hearken] "));
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn test_build_description_wiki_contains_marker() {
        let input = TicketBodyInput {
            template: "ERROR something went wrong".to_string(),
            occurrence_count: 1500,
            first_seen: Some("2024-01-01".to_string()),
            last_seen: Some("2024-06-01".to_string()),
            file_group: "/var/log/app/*.log".to_string(),
            samples: vec!["[ERROR] line 1".to_string(), "[ERROR] line 2".to_string()],
            db_name: "appdb".to_string(),
            pattern_id: 99,
        };
        let wiki = build_description_wiki(&input);

        // Marker is present
        assert!(wiki.contains("hearken:db=appdb;pattern_id=99;occurrences=1500"));
        // Stats
        assert!(wiki.contains("1,500"));
        assert!(wiki.contains("2024-01-01"));
        assert!(wiki.contains("2024-06-01"));
        assert!(wiki.contains("/var/log/app/*.log"));
        // Samples
        assert!(wiki.contains("[ERROR] line 1"));
        // Pattern template
        assert!(wiki.contains("ERROR something went wrong"));
    }

    #[test]
    fn test_build_description_adf_contains_marker() {
        let input = TicketBodyInput {
            template: "WARN low memory".to_string(),
            occurrence_count: 200,
            first_seen: None,
            last_seen: Some("2024-05-15".to_string()),
            file_group: "/var/log/*.log".to_string(),
            samples: vec![],
            db_name: "testdb".to_string(),
            pattern_id: 3,
        };
        let adf = build_description_adf(&input);

        let adf_str = serde_json::to_string(&adf).unwrap();

        // Marker line must appear somewhere in the JSON
        assert!(
            adf_str.contains("hearken:db=testdb;pattern_id=3;occurrences=200"),
            "ADF must contain the marker line"
        );
        // Language attribute on the marker code block
        assert!(adf_str.contains("hearken-metadata"));
    }

    #[test]
    fn test_change_comment_wiki() {
        let comment = build_change_comment_wiki(1000, 1500, "2024-06-01");
        assert!(comment.contains("[hearken sync]"));
        assert!(comment.contains("1,000"));
        assert!(comment.contains("1,500"));
        assert!(comment.contains("+500"));
        assert!(comment.contains("2024-06-01"));
    }

    #[test]
    fn test_change_comment_adf() {
        let adf = build_change_comment_adf(1000, 1500, "2024-06-01");
        let adf_str = serde_json::to_string(&adf).unwrap();
        assert!(adf_str.contains("1,000"));
        assert!(adf_str.contains("1,500"));
        assert!(adf_str.contains("+500"));
        assert!(adf_str.contains("2024-06-01"));
    }
}
